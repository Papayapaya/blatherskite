use chrono::{DateTime, Duration, Local, Utc};
use hmac::{Hmac, digest::typenum::array};
use jwt::{SignWithKey, VerifyWithKey};
use poem::{
    listener::TcpListener, web::Data, EndpointExt, Request, Result,
    Route, Server,
};
use poem_openapi::{
    auth::ApiKey,
    param::Query,
    payload::{Json, PlainText},
    *,
};
use std::sync::Mutex;
use rand::{distributions::Alphanumeric, Rng};
use rustflake::Snowflake;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

pub mod responses;
pub use responses::*;

pub mod db;
pub use db::*;

type ServerKey = Hmac<Sha256>;

/// Struct representing the ID of the authorized users and the expiration date of the token
/// The serialized form of this struct forms the content portion of the JWT returned by /login
#[derive(Serialize, Deserialize)]
struct Claims {
    id: i64,
    exp: DateTime<Local>,
}

/// API key authorization scheme
#[derive(SecurityScheme)]
#[oai(
    type = "api_key",
    key_name = "Authorization", // header to look for API key in
    in = "header",
    checker = "api_checker" // hook to run when checking authorization
)]
struct Authorization(Claims);

/// Check if a user has supplied a valid authorization token.
///
/// Returns None if the token was invalid or if it fails to parse the given token
/// (which will then be handled by Poem to throw a 401), otherwise returns the
/// Claims struct.
async fn api_checker(req: &Request, api_key: ApiKey) -> Option<Claims> {
    let encoded_claims_str = match api_key.key.split(".").nth(1) {
        None => return None,
        Some(s) => s,
    };
    let claims_str = match base64::decode(encoded_claims_str) {
        Err(_) => return None,
        Ok(s) => s,
    };
    let claims: Claims = match serde_json::from_str(&String::from_utf8(claims_str).unwrap()) {
        Err(_) => return None,
        Ok(c) => c
    };          
    if claims.exp < Local::now() {
        return None;
    }    
    let server_key = req.data::<ServerKey>().unwrap(); // get server secret
    VerifyWithKey::<Claims>::verify_with_key(api_key.key.as_str(), server_key).ok()
}

/// Wrapper struct for the API functions
struct Api {
    // The backend.
    db: Box<dyn Database>,  
}

/// Generates a unique i64 for ID generation
// FIXME: Very bad performance - acts as a chokehold for parallelism since
// every request that sends a message / makes a channel / etc. has to contest
// a global mutex.
pub fn gen_id() -> i64 {
    static STATE: Mutex<Option<Snowflake>> = Mutex::new(None);

    STATE
        .lock()
        .unwrap()
        .get_or_insert_with(|| Snowflake::default())
        .generate()
}

#[OpenApi]
#[allow(unused_variables)]
impl Api {
    fn new(db: Box<dyn Database>) -> Api {
        Api { db }
    }

    fn __remove_group_member(&self, gid: i64, uid: i64) {
        self.db.remove_group_member(gid, uid).unwrap();
        let channels = self.db.get_group_channels(gid).unwrap();        
        for channel in channels {
            self.db.remove_channel_member(channel, uid).unwrap();
        }
        self.db.remove_user_group(uid, gid).unwrap();
    }

    #[oai(path = "/login", method = "post")]
    /// Log in as a user. Returns an authentication token given id and hash.
    ///
    /// Expects hash of user's password to be given in the request body.
    /// Checks validity of hash, then signs JWT with a server secret key.
    async fn login(&self, key: Data<&ServerKey>, id: Query<i64>, hash: PlainText<String>) -> LoginResponse {
        use LoginResponse::*;
        if hash.0.len() != 64 {
            return BadRequest;
        } else if !self.db.valid_id(IdType::User, id.0).unwrap() {
            return NotFound;
        }
        let db_hash = self.db.get_user_hash(id.0).unwrap();
        if hex::decode(db_hash.clone()).unwrap() != hex::decode(hash.0.clone()).unwrap() {
            
            Unauthorized
        } else {
            let token = Claims {
                id: id.0,
                exp: Local::now() + Duration::days(1),
            }
            .sign_with_key(key.0);
            Success(PlainText(token.unwrap()))
        }
    }

    #[oai(path = "/user", method = "get")]
    /// Get the user with the given ID
    ///
    /// Does not require any authorization.
    async fn get_user(&self, id: Query<i64>) -> UserResponse {
        use UserResponse::*;
        if !self.db.valid_id(IdType::User, id.0).unwrap() { return NotFound; }
        match self.db.get_user(id.0) {
            Ok(user) => Success(Json(user)),
            Err(e) => InternalError(PlainText(e.to_string()))
        }
    }

    #[oai(path = "/user", method = "post")]
    /// Create a new user.
    ///
    /// Expects hash of user's password to be given in the request body.
    /// Does not require any authorization.
    async fn make_user(&self, name: Query<String>, email: Query<String>, hash: PlainText<String>) -> CreateUserResponse {       
        use CreateUserResponse::*;
        if hash.0.len() != 64 {
            return BadRequest(PlainText("Invalid hash provided.".to_string()));
        }
        
        // name cleaning:
        let disallowed_chars: [u32; 53] = [0x202e,0x0009,0x00AD,0x034F,0x061C,0x115F,0x1160,0x17B4,0x17B5,0x180E,0x2000,0x2001,0x2002,0x2003,0x2004,0x2005,0x2006,0x2007,0x2008,0x2009,0x200A,0x200B,0x200C,0x200D,0x200E,0x200F,0x202F,0x205F,0x2060,0x2061,0x2062,0x2063,0x2064,0x206A,0x206B,0x206C,0x206D,0x206E,0x206F,0x3000,0x2800,0x3164,0xFEFF,0xFFA0,0x1D159,0x1D173,0x1D174,0x1D175,0x1D176,0x1D177,0x1D178,0x1D179,0x1D17A];
        let name_chars = name.chars();
        let mut fixed_name = String::from("");
        let mut index = 0;
        let mut to_keep:[bool; 32] = [true; 32]; //assuming 32 is the max name size - TODO: figure out how to not hardcode this in rust
        for i in name_chars{
            for j in 0..disallowed_chars.len(){
                if i == char::from_u32(disallowed_chars[j]).unwrap() {
                    to_keep[index] = false;
                }; 
            };
            if to_keep[index]{
                fixed_name.push(i)
            };
                index += 1;
        };


        let id = gen_id();
        self.db.create_user(id, name.0.clone(), email.0.clone(), hash.0).unwrap();
        self.db.create_user_groups(id).unwrap();
        self.db.create_user_dms(id).unwrap();
        Success(Json(User {
            id,
            username: name.0,
            email: email.0,
        }))
    }

    #[oai(path = "/user", method = "put")]
    /// Update your name and email.
    async fn update_user(&self, auth: Authorization, name: Query<String>, email: Query<String>) -> GenericResponse {
        use GenericResponse::*;
        self.db.update_user(auth.0.id, name.0, email.0).unwrap();
        Success
    }

    #[oai(path = "/user", method = "delete")]
    /// Delete your user.
    ///
    /// Has the side effects of removing your user from every group, channel, or DM
    /// it is a member of.    
    async fn delete_user(&self, auth: Authorization) -> DeleteResponse {
        use DeleteResponse::*;
        self.db.delete_user(auth.0.id).unwrap();        
        for group in self.db.get_user_groups(auth.0.id).unwrap() {
            self.__remove_group_member(group, auth.0.id);
        }
        for dm in self.db.get_user_dms(auth.0.id).unwrap() {
            self.__remove_group_member(dm, auth.0.id);
        }
        self.db.delete_user_groups(auth.0.id).unwrap();     
        Success
    }

    #[oai(path = "/user/groups", method = "get")]
    /// Get all groups accessible to you.
    async fn get_groups(&self, auth: Authorization) -> GroupsResponse {
        use GroupsResponse::*;
        let groups = self.db.get_user_groups(auth.0.id).unwrap();
        let group_vec = groups.iter().map(|i| {
            self.db.get_group(*i).unwrap()
        }).collect();
        Success(Json(group_vec))
    }

    #[oai(path = "/user/dms", method = "get")]
    /// Get all DMs accessible to you.
    async fn get_dms(&self, auth: Authorization) -> GroupsResponse {
        use GroupsResponse::*;
        let groups = self.db.get_user_dms(auth.0.id).unwrap();
        let group_vec = groups.iter().map(|i| {
            self.db.get_group(*i).unwrap()
        }).collect();
        Success(Json(group_vec))
    }

    
    #[oai(path = "/user/groups", method = "delete")]
    /// Leave a group accessible to you
    async fn leave_group(&self, auth: Authorization, gid: Query<i64>) -> GenericResponse {
        use GenericResponse::*;
        if !self.db.valid_id(IdType::Group, gid.0).unwrap() {
            return NotFound(PlainText("Group not found".to_string()));
        }
        self.__remove_group_member(gid.0, auth.0.id);
        Success
    }

    #[oai(path = "/group", method = "get")]
    /// Gets the group with the given ID
    async fn get_group(&self, auth: Authorization, id: Query<i64>) -> GroupResponse {
        use GroupResponse::*;
        if !self.db.valid_id(IdType::Group, id.0).unwrap() ||
           !self.db.get_group_members(id.0).unwrap().contains(&auth.0.id)
        {
            return NotFound;
        }
        Success(Json(self.db.get_group(id.0).unwrap()))
    }

    #[oai(path = "/group", method = "post")]
    /// Create a new group.
    ///
    /// The group created...
    /// - will have a default public "main" channel
    /// - will have your user as the owner
    /// - will have your user as an admin 
    async fn make_group(&self, auth: Authorization, name: Query<String>) -> CreateGroupResponse {
        use CreateGroupResponse::*;
        let gid = gen_id();
        if name.0 == "" {
            return BadRequest(PlainText("Empty string not allowed for name".to_string()))
        }
        self.db.create_group(gid, auth.0.id, name.0.clone(), false).unwrap();
        self.db.add_user_group(auth.0.id, gid).unwrap();
        self.db.add_group_admin(gid, auth.0.id).unwrap();
        let cid = gen_id();
        self.db.create_channel(cid, gid, auth.0.id, String::from("main")).unwrap();
        self.db.add_group_channel(gid, cid).unwrap();
        Success(Json(Group {
            id: gid,
            name: name.0,
            members: vec![auth.0.id],
            channels: vec![cid],
            admin: vec![auth.0.id],
            owner: auth.0.id,
            is_dm: false
        }))
    }
    
    #[oai(path = "/dm", method = "post")]
    /// Create a new DM with a user `uid`.
    ///
    /// The group created...
    /// - will have the `is_dm` attribute set to true.
    /// - will have only one channel "main" with you and `uid`
    /// - will have no owner or admins
    async fn make_dm(&self, auth: Authorization, uid: Query<i64>) -> CreateGroupResponse {       
        use CreateGroupResponse::*;
        if !self.db.valid_id(IdType::User, uid.0).unwrap() {
            return NotFound;
        }
        let gid = gen_id();
        self.db.create_group(gid, auth.0.id, String::from(""), true).unwrap();
        self.db.add_group_member(gid, uid.0).unwrap();
        self.db.add_user_dm(auth.0.id, gid).unwrap();
        self.db.add_user_dm(uid.0, gid).unwrap();
        let cid = gen_id();
        self.db.create_channel(cid, gid, auth.0.id, String::from("main")).unwrap();
        self.db.add_group_channel(gid, cid).unwrap();
        self.db.add_channel_member(cid, uid.0).unwrap();
        Success(Json(Group {
            id: gid,
            name: String::from(""),
            members: vec![auth.0.id, uid.0],
            channels: vec![cid],
            admin: vec![],
            owner: auth.0.id,
            is_dm: true
        }))
    }
    
    #[oai(path = "/group", method = "put")]
    /// Update the name of an existing group.
    ///
    /// Only authorized for the owner of a group.
    async fn update_group(&self, auth: Authorization, id: Query<i64>, name: Query<String>) -> GenericResponse {
        use GenericResponse::*;
        if name.0 == "" {
            return BadRequest(PlainText("Empty string not allowed for name".to_string()))
        } else if !self.db.valid_id(IdType::Group, id.0).unwrap() {
            return NotFound(PlainText("Didn't find group or experienced database error.".to_string()));
        } else if self.db.get_group_owner(id.0).unwrap() != auth.0.id {
            return Unauthorized;
        }        
        self.db.update_group(id.0, name.0).unwrap();
        Success
    }

    #[oai(path = "/group", method = "delete")]
    /// Delete a group. 
    /// 
    /// Only authorized for the owner of a group.
    async fn delete_group(&self, auth: Authorization, id: Query<i64>) -> DeleteResponse {
        use DeleteResponse::*;
        if !self.db.valid_id(IdType::Group, id.0).unwrap() {
            return NotFound(PlainText("Group not found".to_string()));
        } else if self.db.get_group_owner(id.0).unwrap() != auth.0.id {
            return Unauthorized;
        }
        let group = self.db.get_group(id.0).unwrap();
        for member in group.members {
            self.db.remove_user_group(member, id.0).unwrap();
        }
        for channel in group.channels {
            self.db.delete_channel(channel).unwrap();
        }
        self.db.delete_group(id.0).unwrap();
        Success
    }

    #[oai(path = "/group/members", method = "get")]
    /// Get the members of the specified group.
    ///
    /// No specific order for the list is guaranteed.
    async fn get_group_members(&self, auth: Authorization, id: Query<i64>) -> MembersResponse {
        use MembersResponse::*;
        if !self.db.valid_id(IdType::Group, id.0).unwrap() {
            return NotFound;
        }       
        let members = self.db.get_group_members(id.0).unwrap();
        Success(Json(members.iter().map(|m| {
            self.db.get_user(*m).unwrap()
        }).collect::<Vec<User>>()))     
    }

    #[oai(path = "/group/members", method = "put")]
    /// Add a member to an existing group
    ///
    /// Only authorized for group admins.
    /// Has the side effect of adding that member to all public channels.
    async fn add_group_member(&self, auth: Authorization, gid: Query<i64>, uid: Query<i64>) -> GenericResponse {
        use GenericResponse::*;
        if !self.db.valid_id(IdType::Group, gid.0).unwrap() {
            return NotFound(PlainText("Group not found".to_string()));
        } else if !self.db.get_group_admin(gid.0).unwrap().contains(&auth.0.id) &&
            self.db.get_group_owner(gid.0).unwrap() != auth.0.id
        {
            return Unauthorized;
        }
        self.db.add_group_member(gid.0, uid.0).unwrap();
        let channels = self.db.get_group_channels(gid.0).unwrap();
        for channel in channels {
            if self.db.is_channel_private(channel).unwrap() { continue; }
            self.db.add_channel_member(channel, uid.0).unwrap();
        }
        if !self.db.is_group_dm(gid.0).unwrap() {
            self.db.add_user_group(uid.0, gid.0).unwrap();
        } else {
            self.db.add_user_dm(uid.0, gid.0).unwrap();
        }
        Success
    }

    #[oai(path = "/group/members", method = "delete")]
    /// Remove a member from an existing group
    ///
    /// Only authorized for group admin.
    /// Attempting to remove the owner from their group will always be unauthorized.
    /// 
    /// Has the side effect of removing the member from all channels.    
    async fn remove_group_member(&self, auth: Authorization, gid: Query<i64>, uid: Query<i64>) -> DeleteResponse {
        use DeleteResponse::*;
        if !self.db.valid_id(IdType::Group, gid.0).unwrap() {
            return NotFound(PlainText("Group not found".to_string()))
        } else if !self.db.valid_id(IdType::User, uid.0).unwrap() {
            return NotFound(PlainText("User not found".to_string()))
        } else if !self.db.get_group_admin(gid.0).unwrap().contains(&auth.0.id)
            || self.db.get_group_owner(gid.0).unwrap() == uid.0
        {            
            return Unauthorized;
        }
        self.__remove_group_member(gid.0, uid.0);
        Success
    }

    #[oai(path = "/group/admin", method = "get")]
    /// Get the admins of the specified group.
    ///
    /// No specific order for the list is guaranteed.
    async fn get_group_admin(&self, auth: Authorization, id: Query<i64>) -> MembersResponse {
        use MembersResponse::*;
        if !self.db.valid_id(IdType::Group, id.0).unwrap() {
            return NotFound;
        }       
        let members = self.db.get_group_admin(id.0).unwrap();
        Success(Json(members.iter().map(|m| {
            self.db.get_user(*m).unwrap()
        }).collect::<Vec<User>>()))     
    }

    #[oai(path = "/group/admin", method = "put")]
    /// Add an admin to an existing group
    ///
    /// Only authorized for the owner of a group.
    async fn add_group_admin(&self, auth: Authorization, gid: Query<i64>, uid: Query<i64>) -> GenericResponse {
        use GenericResponse::*;
        if !self.db.valid_id(IdType::Group, gid.0).unwrap() {
            return NotFound(PlainText("Group not found".to_string()));
        } else if !self.db.valid_id(IdType::User, uid.0).unwrap() {
            return NotFound(PlainText("User not found".to_string()))
        } else if self.db.get_group_owner(gid.0).unwrap() != auth.0.id {
            return Unauthorized;
        }
        self.db.add_group_admin(gid.0, uid.0).unwrap();  
        Success
    }

    #[oai(path = "/group/admin", method = "delete")]
    /// Remove an admin from an existing group
    ///
    /// Only authorized for the owner of a group.
    async fn remove_group_admin(&self, auth: Authorization, gid: Query<i64>, uid: Query<i64>) -> DeleteResponse {
        use DeleteResponse::*;
        if !self.db.valid_id(IdType::Group, gid.0).unwrap() {
            return NotFound(PlainText("Group not found".to_string()))
        } else if !self.db.valid_id(IdType::User, uid.0).unwrap() {
            return NotFound(PlainText("User not found".to_string()))
        } else if self.db.get_group_owner(gid.0).unwrap() != auth.0.id {
            return Unauthorized;
        }
        self.db.remove_group_admin(gid.0, uid.0).unwrap();
        Success
    }
    
    #[oai(path = "/group/channels", method = "get")]
    /// Gets all channels in a group that are accessible to you
    async fn get_channels(&self, auth: Authorization, gid: Query<i64>) -> ChannelsResponse {
        use ChannelsResponse::*;
        if !self.db.valid_id(IdType::Group, gid.0).unwrap() {
            return NotFound;
        }
        let channels = self.db.get_group_channels(gid.0).unwrap();
        Success(Json(channels.iter().map(|c| {
            self.db.get_channel(*c).unwrap()
        }).filter(|c| c.members.contains(&auth.0.id)).collect::<Vec<Channel>>()))
    }

    #[oai(path = "/group/channels", method = "post")]
    /// Create a channel in a group.
    ///
    /// Only authorized for a group admin.
    /// Defaults to a public channel with no members but yourself.
    // TODO add some mechanism for auto-inviting current members
    async fn make_channel(&self, auth: Authorization, gid: Query<i64>, name: Query<String>) -> CreateChannelResponse {
        use CreateChannelResponse::*;
        if name.0 == "" {
            return BadRequest(PlainText("Empty string not allowed for name".to_string()))
        } else if !self.db.valid_id(IdType::Group, gid.0).unwrap() {
            return NotFound(PlainText("Group not found".to_string()));
        } else if !self.db.get_group_admin(gid.0).unwrap().contains(&auth.0.id) {
            return Unauthorized;
        }
        let cid = gen_id();
        self.db.create_channel(cid, gid.0, auth.0.id, name.0.clone()).unwrap();
        self.db.add_group_channel(gid.0, cid).unwrap();
        Success(Json(Channel {
            id: cid,
            group: gid.0,
            name: name.0,
            members: vec![auth.0.id],
            private: false
        }))
    }

    #[oai(path = "/channel", method = "put")]
    /// Update the name of a channel.
    ///
    /// Only authorized for group admins.
    async fn update_channel(&self, auth: Authorization, id: Query<i64>, name: Query<String>) -> GenericResponse {
        use GenericResponse::*;
        if !self.db.valid_id(IdType::Channel, id.0).unwrap() {
            return NotFound(PlainText("Channel not found".to_string()));
        }
        let channel = self.db.get_channel(id.0).unwrap();
        if !self.db.get_group_admin(channel.group).unwrap().contains(&auth.0.id) {
            return Unauthorized;
        }
        self.db.update_channel(id.0, name.0).unwrap();
        Success
    }
    
    #[oai(path = "/channel/private", method = "put")]
    /// Make a channel private.
    ///
    /// Only authorized for group admins.
    async fn make_channel_private(&self, auth: Authorization, id: Query<i64>, val: Query<bool>) -> GenericResponse {
        use GenericResponse::*;
        if !self.db.valid_id(IdType::Channel, id.0).unwrap() {
            return NotFound(PlainText("Channel not found".to_string()));
        }
        let channel = self.db.get_channel(id.0).unwrap();
        if !self.db.get_group_admin(channel.group).unwrap().contains(&auth.0.id) {
            return Unauthorized;
        }
        self.db.set_channel_private(id.0, val.0).unwrap();
        Success
    }
    
    #[oai(path = "/channel", method = "get")]
    /// Get a channel.
    async fn get_channel(&self, auth: Authorization, id: Query<i64>) -> ChannelResponse {
        use ChannelResponse::*;
        if !self.db.valid_id(IdType::Channel, id.0).unwrap() ||
           !self.db.get_channel_members(id.0).unwrap().contains(&auth.0.id)
        {
            return NotFound;
        }
        Success(Json(self.db.get_channel(id.0).unwrap()))
    }

    #[oai(path = "/channel", method = "delete")]
    /// Delete a channel.
    ///
    /// Only authorized for group admins.
    async fn delete_channel(&self, auth: Authorization, id: Query<i64>) -> DeleteResponse {
        use DeleteResponse::*;
        if !self.db.valid_id(IdType::Channel, id.0).unwrap() {
            return NotFound(PlainText("Channel not found".to_string()));
        }
        let channel = self.db.get_channel(id.0).unwrap();        
        if !self.db.get_group_admin(channel.group).unwrap().contains(&auth.0.id) {
            return Unauthorized;
        }
        self.db.remove_group_channel(channel.group, id.0).unwrap();
        self.db.delete_channel(id.0).unwrap();
        Success
    }

    #[oai(path = "/channel/members", method = "get")]
    /// Get the members that can access a channel.
    ///
    /// No specific order for the list is guaranteed.
    async fn get_channel_members(&self, auth: Authorization, id: Query<i64>) -> MembersResponse {
        use MembersResponse::*;
        let members = self.db.get_channel_members(id.0).unwrap();
        Success(Json(members.iter().map(|m| {
            self.db.get_user(*m).unwrap()
        }).collect::<Vec<User>>()))
    }

    #[oai(path = "/channel/members", method = "put")]
    /// Add a member to a channel
    ///
    /// Only authorized for group admins.
    async fn add_channel_member(&self, auth: Authorization, cid: Query<i64>, uid: Query<i64>) -> GenericResponse {
        use GenericResponse::*;
        if !self.db.valid_id(IdType::Channel, cid.0).unwrap() {
            return NotFound(PlainText("Channel not found".to_string()))
        } else if !self.db.valid_id(IdType::User, uid.0).unwrap() {
            return NotFound(PlainText("User not found".to_string()))
        }
        let channel = self.db.get_channel(cid.0).unwrap();        
        if !self.db.get_group_admin(channel.group).unwrap().contains(&auth.0.id) {
            return Unauthorized;
        }
        self.db.add_channel_member(cid.0, uid.0).unwrap();
        Success
    }

    #[oai(path = "/channel/members", method = "delete")]
    /// Remove a member from a channel.
    ///
    /// Only authorized for group admins.
    async fn remove_channel_member(&self, auth: Authorization, cid: Query<i64>, uid: Query<i64>) -> DeleteResponse {
        use DeleteResponse::*;
        if !self.db.valid_id(IdType::Channel, cid.0).unwrap() {
            return NotFound(PlainText("Channel not found".to_string()))
        } else if !self.db.valid_id(IdType::User, uid.0).unwrap() {
            return NotFound(PlainText("User not found".to_string()))
        }
        let channel = self.db.get_channel(cid.0).unwrap();        
        if !self.db.get_group_admin(channel.group).unwrap().contains(&auth.0.id) {
            return Unauthorized;
        }
        self.db.remove_channel_member(cid.0, uid.0).unwrap();
        Success
    }

    #[oai(path = "/channel/term", method = "get")]
    /// Get a batch of messages in channel containing `term` in the last 100 messages
    ///
    /// Will not search for `term` in any messages older than the last 100.
    async fn search_channel(&self, auth: Authorization, cid: Query<i64>, term: Query<String>, off: Query<u64>) -> MessagesResponse {
        use MessagesResponse::*;
        if !self.db.valid_id(IdType::Channel, cid.0).unwrap() {
            return NotFound(PlainText("Channel not found".to_string()))
        }
        let mut messages = self.db.get_messages(cid.0, 100).unwrap();
        messages.retain(|msg| msg.content.contains(&term.0));
        Success(Json(messages)) 
    }

    #[oai(path = "/channel/messages", method = "get")]
    /// Returns batch of messages in channel. Do not use for small batches.
    ///
    /// For small batches, use `chatterbox`, the websocket service for messaging, instead.
    async fn get_channel_messages(&self, auth: Authorization, cid: Query<i64>, num_msgs: Query<u64>) -> MessagesResponse {
        use MessagesResponse::*;
        if !self.db.valid_id(IdType::Channel, cid.0).unwrap() {
            return NotFound(PlainText("Channel not found".to_string()))
        }
        Success(Json(self.db.get_messages(cid.0, num_msgs.0).unwrap()))
    }

    #[oai(path = "/message/thread", method = "put")]
    /// Make a thread for a given message.
    ///
    /// Thread will be private with you as its sole member
    async fn make_thread(&self, auth: Authorization, id: Query<i64>, name: Query<String>) -> CreateChannelResponse {
        use CreateChannelResponse::*;
        if name.0 == "" {
            return BadRequest(PlainText("Empty string not allowed for name".to_string()))
        } else if !self.db.valid_id(IdType::Message, id.0).unwrap() {
            return NotFound(PlainText("Message not found".to_string()))
        }
        let tid = gen_id();
        let msg = self.db.get_message(id.0).unwrap();
        let chan = self.db.get_channel(msg.channel).unwrap();
        self.db.create_channel(tid, chan.group, auth.0.id, name.0.clone()).unwrap();
        self.db.set_channel_private(tid, true).unwrap();
        self.db.set_thread(id.0, tid).unwrap();
        Success(Json(Channel {
            id: tid,
            group: chan.group,
            members: vec![auth.0.id],
            name: name.0,
            private: true
        }))
    }

    #[oai(path = "/message", method = "delete")]
    /// Delete a message
    ///
    /// Only authorized for the message author or a group admin.
    async fn delete_message(&self, auth: Authorization, id: Query<i64>) -> DeleteResponse {
        use DeleteResponse::*;
        if !self.db.valid_id(IdType::Message, id.0).unwrap() {
            return NotFound(PlainText("Message not found".to_string()))
        }
        let msg = self.db.get_message(id.0).unwrap();
        let chan = self.db.get_channel(msg.channel).unwrap();
        if msg.author != auth.0.id && !self.db.get_group_admin(chan.group).unwrap().contains(&auth.0.id) {
            return Unauthorized;
        }
        self.db.delete_message(id.0).unwrap();
        Success
    }
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    use hmac::Mac;
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "poem=debug");
    }
    tracing_subscriber::fmt::init();

    let db = Box::new(Cassandra::new("bsk"));
    let api_service = OpenApiService::new(Api::new(db), "Scuttlebutt", "1.0")
        .description(
            "Scuttlebutt is the REST API for managing everything but sending/receiving messages \
                      - which means creating/updating/deleting all of your users/groups/channels.",
        )
        .server("http://localhost:3000/api");

    // API documentation 
    let ui = api_service.swagger_ui();

    // Generate server-side secret key used for signing the JWTs
    let key: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(7)
        .map(char::from)
        .collect();

    let app = Route::new()
        .nest("/api", api_service)
        .nest("/", ui)
        .data(ServerKey::new_from_slice(&key.as_bytes()).unwrap());

    Server::new(TcpListener::bind("127.0.0.1:3000")).run(app).await
}

#[cfg(test)]
mod tests;
