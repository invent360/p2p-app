
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use actix::prelude::*;
use rand::Rng as _;

#[derive(Message)]
#[rtype(result = "()")]
pub struct ActorMessage(pub String);


#[derive(Message)]
#[rtype(u64)]
pub struct Connect {
    pub addr: Recipient<ActorMessage>,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Disconnect {
    pub id: u64,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct ClientMessage {
    pub id: u64,
    pub msg: String,
    pub room: String,
}

pub struct ListRooms;

impl actix::Message for ListRooms {
    type Result = Vec<String>;
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Join {
    pub id: u64,
    pub name: String,
}

#[derive(Debug)]
pub struct RoomActor {
    sessions: HashMap<u64, Recipient<ActorMessage>>,
    rooms: HashMap<String, HashSet<u64>>,
    visitor_count: Arc<AtomicUsize>,
}

impl RoomActor {
    pub fn new(visitor_count: Arc<AtomicUsize>) -> RoomActor {
        let mut rooms = HashMap::new();
        rooms.insert("main".to_owned(), HashSet::new());

        RoomActor {
            sessions: HashMap::new(),
            rooms,
            visitor_count,
        }
    }
}

impl RoomActor {
    fn send_message(&self, room: &str, message: &str, skip_id: u64) {
        if let Some(sessions) = self.rooms.get(room) {
            for id in sessions {
                if *id != skip_id {
                    if let Some(addr) = self.sessions.get(id) {
                        addr.do_send(ActorMessage(message.to_owned()));
                    }
                }
            }
        }
    }
}

impl Actor for RoomActor {
    type Context = Context<Self>;
}


impl Handler<Connect> for RoomActor {
    type Result = u64;

    fn handle(&mut self, msg: Connect, _: &mut Context<Self>) -> Self::Result {
        println!("Someone joined");

        self.send_message("main", "Someone joined", 0);

        let id = rand::rng().random::<u64>();
        self.sessions.insert(id, msg.addr);

        self.rooms.entry("main".to_owned()).or_default().insert(id);

        let count = self.visitor_count.fetch_add(1, Ordering::SeqCst);
        self.send_message("main", &format!("Total visitors {count}"), 0);

        id
    }
}

impl Handler<Disconnect> for RoomActor {
    type Result = ();

    fn handle(&mut self, msg: Disconnect, _: &mut Context<Self>) {
        println!("Someone disconnected");

        let mut rooms: Vec<String> = Vec::new();

        if self.sessions.remove(&msg.id).is_some() {
            for (name, sessions) in &mut self.rooms {
                if sessions.remove(&msg.id) {
                    rooms.push(name.to_owned());
                }
            }
        }
        for room in rooms {
            self.send_message(&room, "Someone disconnected", 0);
        }
    }
}

impl Handler<ClientMessage> for RoomActor {
    type Result = ();

    fn handle(&mut self, msg: ClientMessage, _: &mut Context<Self>) {
        self.send_message(&msg.room, msg.msg.as_str(), msg.id);
    }
}

impl Handler<ListRooms> for RoomActor {
    type Result = MessageResult<ListRooms>;

    fn handle(&mut self, _: ListRooms, _: &mut Context<Self>) -> Self::Result {
        let mut rooms = Vec::new();

        for key in self.rooms.keys() {
            rooms.push(key.to_owned())
        }

        MessageResult(rooms)
    }
}

impl Handler<Join> for RoomActor {
    type Result = ();

    fn handle(&mut self, msg: Join, _: &mut Context<Self>) {
        let Join { id, name } = msg;
        let mut rooms = Vec::new();

        for (n, sessions) in &mut self.rooms {
            if sessions.remove(&id) {
                rooms.push(n.to_owned());
            }
        }
        for room in rooms {
            self.send_message(&room, "Someone disconnected", 0);
        }

        self.rooms.entry(name.clone()).or_default().insert(id);

        self.send_message(&name, "Someone connected", id);
    }
}
