use mongodb::ThreadedClient as _;
use mongodb::db::ThreadedDatabase as _;
use mongodb::coll::options::FindOptions;
use bson::{doc, bson};

use redis::Commands as _;

use cookie::Cookie;
use serde::{Serialize, Deserialize};
use serde_json::Value as JsonValue;

use ws::{Handshake, Handler, Sender, Message, CloseCode};
use ws::util::Token;
use mio_extras::timer::Timeout;

use structopt::StructOpt;

use std::collections::{HashMap, HashSet};
use std::str;
use std::cmp::max;
use std::sync::atomic::{AtomicI32, Ordering};
use once_cell::sync::OnceCell;
use parking_lot::RwLock;
use crossbeam::channel;

#[derive(StructOpt, Clone)]
struct Opt {
    /// Binding address of Websocket server
    #[structopt(default_value = "127.0.0.1:9664")]
    bind: String,
    /// URI of redis server
    #[structopt(default_value = "redis://127.0.0.1/")]
    redis: String,
    /// URI of mongodb with security collection
    #[structopt(default_value = "mongodb://127.0.0.1/")]
    mongodb: String,
    /// Hard limit for maximum number of simultaneous Websocket connections
    #[structopt(default_value = "40_000")]
    max_connections: usize,
}

/// Messages we send to lila.
#[derive(Serialize)]
#[serde(tag = "path")]
enum LilaIn {
    #[serde(rename = "connect")]
    Connect { user: String },
    #[serde(rename = "disconnect")]
    Disconnect { user: String },
    #[serde(rename = "notified")]
    Notified { user: String },
    #[serde(rename = "watch")]
    Watch { game: String },
    #[serde(rename = "count")]
    Count { value: u32 },
}

/// Messages we receive from lila.
#[derive(Deserialize)]
#[serde(tag = "path")]
enum LilaOut {
    #[serde(rename = "move")]
    Move {
        #[serde(rename = "gameId")]
        game_id: String,
        fen: String,
        #[serde(rename = "move")]
        m: String,
    },
    #[serde(rename = "tell/user")]
    Tell {
        user: String,
        payload: JsonValue,
    },
    #[serde(rename = "tell/users")]
    TellMany {
        users: Vec<String>,
        payload: JsonValue,
    },
    #[serde(rename = "tell/all")]
    TellAll {
        payload: JsonValue,
    },
    #[serde(rename = "mlat")]
    MoveLatency {
        value: u32,
    },
}

/// Messages we send to Websocket clients.
#[derive(Serialize)]
#[serde(tag = "t", content = "d")]
enum SocketIn<'a> {
    #[serde(rename = "fen")]
    Fen {
        id: &'a str,
        fen: &'a str,
        lm: &'a str,
    },
    #[serde(rename = "mlat")]
    MoveLatency { d: u32 },
}

/// Messages we receive from Websocket clients.
#[derive(Deserialize)]
#[serde(tag = "t")]
enum SocketOut {
    #[serde(rename = "p")]
    Ping { #[allow(unused)] l: u32 },
    #[serde(rename = "notified")]
    Notified,
    #[serde(rename = "startWatching")]
    StartWatching { d: String },
    #[serde(rename = "moveLat")]
    MoveLatency { d: bool },
}

/// Session cookie from Play framework.
#[derive(Debug, Deserialize)]
struct SessionCookie {
    #[serde(rename = "sessionId")]
    session_id: String,
}

/// Token for the timeout that's used to close Websockets after some time
/// of inactivity.
const IDLE_TIMEOUT: Token = Token(1);

/// State of this Websocket server.
struct App {
    by_user: RwLock<HashMap::<String, Vec<Sender>>>,
    by_game: RwLock<HashMap::<String, Vec<Sender>>>,
    watching_mlat: RwLock<HashSet<Sender>>,
    redis_sink: channel::Sender<LilaIn>,
    session_store: mongodb::coll::Collection,
    broadcaster: OnceCell<Sender>,
    connection_count: AtomicI32, // signed to allow relaxed writes with underflow
}

impl App {
    fn new(redis_sink: channel::Sender<LilaIn>, session_store: mongodb::coll::Collection) -> App {
        App {
            by_user: RwLock::new(HashMap::new()),
            by_game: RwLock::new(HashMap::new()),
            redis_sink,
            session_store,
            broadcaster: OnceCell::new(),
            connection_count: AtomicI32::new(0),
            watching_mlat: RwLock::new(HashSet::new()),
        }
    }

    fn publish(&self, msg: LilaIn) {
        self.redis_sink.send(msg).expect("redis sink");
    }

    fn received(&self, msg: LilaOut) {
        match msg {
            LilaOut::Tell { user, payload } => {
                let by_user = self.by_user.read();
                if let Some(entry) = by_user.get(&user) {
                    for sender in entry {
                        if let Err(err) = sender.send(Message::text(payload.to_string())) {
                            log::warn!("failed to tell ({}): {:?}", user, err);
                        }
                    }
                }
            }
            LilaOut::TellMany { users, payload } => {
                let by_user = self.by_user.read();
                for user in &users {
                    if let Some(entry) = by_user.get(user) {
                        for sender in entry {
                            if let Err(err) = sender.send(Message::text(payload.to_string())) {
                                log::warn!("failed to tell ({}): {:?}", user, err);
                            }
                        }
                    }
                }
            }
            LilaOut::TellAll { payload } => {
                let msg = serde_json::to_string(&payload).expect("serialize for broadcast");
                if let Err(err) = self.broadcaster.get().expect("broadcaster").send(msg) {
                    log::warn!("failed to send broadcast: {:?}", err);
                }
            }
            LilaOut::Move { ref game_id, ref fen, ref m } => {
                let by_game = self.by_game.read();
                if let Some(entry) = by_game.get(game_id) {
                    let msg = Message::text(serde_json::to_string(&SocketIn::Fen {
                        id: game_id,
                        fen,
                        lm: m,
                    }).expect("serialize fen"));

                    for sender in entry {
                        if let Err(err) = sender.send(msg.clone()) {
                            log::warn!("failed to send fen: {:?}", err);
                        }
                    }
                }
            }
            LilaOut::MoveLatency { value } => {
                // Respond with our stats (connection count).
                self.publish(LilaIn::Count {
                    value: max(0, self.connection_count.load(Ordering::Relaxed)) as u32
                });

                // Update watching clients.
                let msg = serde_json::to_string(&SocketIn::MoveLatency { d: value }).expect("serialize mlat");
                let watching_mlat = self.watching_mlat.read();
                for sender in watching_mlat.iter() {
                    if let Err(err) = sender.send(msg.clone()) {
                        log::warn!("failed to send mlat: {:?}", err);
                    }
                }
            }
        }
    }
}

/// A Websocket client connection.
struct Socket {
    app: &'static App,
    sender: Sender,
    uid: Option<String>,
    watching: HashSet<String>,
    idle_timeout: Option<Timeout>,
}

impl Handler for Socket {
    fn on_open(&mut self, handshake: Handshake) -> ws::Result<()> {
        // Update connection count.
        self.app.connection_count.fetch_add(1, Ordering::Relaxed);

        // Ask mongodb for user id based on session cookie.
        self.uid = handshake.request.header("cookie")
            .and_then(|h| str::from_utf8(h).ok())
            .and_then(|h| {
                h.split(';')
                    .map(|p| p.trim())
                    .filter(|p| p.starts_with("lila2="))
                    .next()
            })
            .and_then(|h| Cookie::parse(h).ok())
            .and_then(|c| {
                let s = c.value();
                let idx = s.find('-').map(|n| n + 1).unwrap_or(0);
                serde_urlencoded::from_str::<SessionCookie>(&s[idx..]).ok()
            })
            .and_then(|c| {
                let query = doc! { "_id": c.session_id, "up": true, };
                let mut opts = FindOptions::new();
                opts.projection = Some(doc! { "user": true });
                match self.app.session_store.find_one(Some(query), Some(opts)) {
                    Ok(Some(doc)) => doc.get_str("user").map(|s| s.to_owned()).ok(),
                    Ok(None) => {
                        log::info!("session store lookup with expired sid");
                        None
                    }
                    Err(err) => {
                        log::error!("session store query failed: {:?}", err);
                        None
                    }
                }
            });

        // Add socket to by_user map.
        if let Some(ref uid) = self.uid {
            self.app.by_user.write()
                .entry(uid.to_owned())
                .and_modify(|v| v.push(self.sender.clone()))
                .or_insert_with(|| {
                    log::debug!("first open: {}", uid);
                    self.app.publish(LilaIn::Connect { user: uid.to_owned() });
                    vec![self.sender.clone()]
                });
        }

        // Start idle timeout.
        self.sender.timeout(10_000, IDLE_TIMEOUT)
    }

    fn on_close(&mut self, _: CloseCode, _: &str) {
        // Update connection count. (Due to relaxed ordering this can
        // temporarily be less than 0).
        self.app.connection_count.fetch_sub(1, Ordering::Relaxed);

        // Clear timeout.
        if let Some(timeout) = self.idle_timeout.take() {
            if let Err(err) = self.sender.cancel(timeout) {
                log::error!("failed to clear timeout: {:?}", err);
            }
        }

        // Update by_user.
        if let Some(uid) = self.uid.take() {
            let mut by_user = self.app.by_user.write();
            let entry = by_user.get_mut(&uid).expect("uid in by_user");
            let idx = entry.iter().position(|s| s.token() == self.sender.token()).expect("sender in by_user entry");
            entry.swap_remove(idx);

            // Last remaining connection closed.
            if entry.is_empty() {
                by_user.remove(&uid);
                log::debug!("last close: {}", uid);
                self.app.publish(LilaIn::Disconnect { user: uid });
            }
        }

        // Update by_game.
        let mut by_game = self.app.by_game.write();
        let our_token = self.sender.token();
        for game in self.watching.drain() {
            let watchers = by_game.get_mut(&game).expect("game in by_game");
            let idx = watchers.iter().position(|s| s.token() == our_token).expect("sender in watchers");
            watchers.swap_remove(idx);
            if watchers.is_empty() {
                by_game.remove(&game);
            }
        }
    }

    fn on_message(&mut self, msg: Message) -> ws::Result<()> {
        self.sender.timeout(10_000, IDLE_TIMEOUT)?;

        let msg = msg.as_text()?;
        if msg == "null" {
            // Fast path for ping.
            return self.sender.send(Message::text("0"));
        }

        match serde_json::from_str(msg) {
            Ok(SocketOut::Ping { .. }) => {
                self.sender.send(Message::text("0"))
            }
            Ok(SocketOut::Notified) => {
                if let Some(ref uid) = self.uid {
                    log::debug!("notified: {}", uid);
                    self.app.publish(LilaIn::Notified { user: uid.to_owned() });
                }
                Ok(())
            }
            Ok(SocketOut::StartWatching { d }) => {
                if self.watching.insert(d.clone()) {
                    self.app.by_game.write()
                        .entry(d.clone())
                        .and_modify(|v| v.push(self.sender.clone()))
                        .or_insert_with(|| {
                            log::debug!("start watching: {}", d);
                            self.app.publish(LilaIn::Watch { game: d });
                            vec![self.sender.clone()]
                        });
                }
                Ok(())
            },
            Ok(SocketOut::MoveLatency { d }) => {
                let mut watching_mlat = self.app.watching_mlat.write();
                if d {
                    watching_mlat.insert(self.sender.clone());
                } else {
                    watching_mlat.remove(&self.sender);
                }
                Ok(())
            },
            Err(err) => {
                log::warn!("protocol violation of client: {:?}", err);
                self.sender.close(CloseCode::Protocol)
            }
        }
    }

    fn on_new_timeout(&mut self, event: Token, timeout: Timeout) -> ws::Result<()> {
        assert_eq!(event, IDLE_TIMEOUT);
        if let Some(old_timeout) = self.idle_timeout.take() {
            self.sender.cancel(old_timeout)?;
        }
        self.idle_timeout = Some(timeout);
        Ok(())
    }

    fn on_timeout(&mut self, event: Token) -> ws::Result<()> {
        assert_eq!(event, IDLE_TIMEOUT);
        log::info!("closing socket due to timeout");
        self.sender.close(CloseCode::Away)
    }
}

fn main() {
    env_logger::init();

    crossbeam::scope(|s| {
        let opt = Opt::from_args();

        let session_store = mongodb::Client::with_uri(&opt.mongodb)
            .expect("mongodb connect")
            .db("lichess")
            .collection("security");

        let (redis_sink, redis_recv) = channel::unbounded();
        let app: &'static App = Box::leak(Box::new(App::new(redis_sink, session_store)));

        // Thread for outgoing messages to lila.
        let opt_inner = opt.clone();
        s.spawn(move |_| {
            let redis = redis::Client::open(opt_inner.redis.as_str())
                .expect("redis open for publish")
                .get_connection()
                .expect("redis connection for publish");

            loop {
                let msg = redis_recv.recv().expect("redis recv");
                let msg = serde_json::to_string(&msg).expect("serialize site-in");
                log::debug!("site-in: {}", msg);
                let ret: u32 = redis.publish("site-in", msg).expect("publish site-in");
                if ret == 0 {
                    log::error!("lila missed as message");
                }
            }
        });

        // Thread for incoming messages from lila.
        let opt_inner = opt.clone();
        s.spawn(move |_| {
            let mut redis = redis::Client::open(opt_inner.redis.as_str())
                .expect("redis open for subscribe")
                .get_connection()
                .expect("redis connection for subscribe");

            let mut incoming = redis.as_pubsub();
            incoming.subscribe("site-out").expect("subscribe site-out");

            loop {
                let redis_msg = incoming.get_message().expect("get message");
                let payload: String = redis_msg.get_payload().expect("get payload");
                log::debug!("site-out: {}", payload);
                let msg: LilaOut = serde_json::from_str(&payload).expect("deserialize site-out");
                app.received(msg);
            }
        });

        // Start websocket server.
        let mut settings = ws::Settings::default();
        settings.max_connections = opt.max_connections;
        settings.tcp_nodelay = true;

        let server = ws::Builder::new()
            .with_settings(settings)
            .build(move |sender| {
                Socket {
                    app,
                    sender,
                    uid: None,
                    watching: HashSet::new(),
                    idle_timeout: None
                }
            })
            .expect("valid settings");

        app.broadcaster.set(server.broadcaster()).expect("set broadcaster");

        server
            .listen(&opt.bind)
            .expect("ws listen");
    }).expect("scope");
}
