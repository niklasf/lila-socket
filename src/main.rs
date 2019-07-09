use mongodb::ThreadedClient as _;
use mongodb::db::ThreadedDatabase as _;
use mongodb::coll::options::FindOptions;
use bson::{doc, bson};

use redis::Commands as _;

use cookie::Cookie;
use serde::{Serialize, Deserialize};

use ws::{Handshake, Handler, Sender, Message, CloseCode};
use ws::util::Token;
use mio_extras::timer::Timeout;

use structopt::StructOpt;

use std::str;
use std::mem;
use std::cmp::max;
use std::collections::{HashMap, HashSet};
use smallvec::SmallVec;

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use once_cell::sync::OnceCell;
use parking_lot::RwLock;
use lru::LruCache;
use crossbeam::channel;

mod model;
mod ipc;
mod util;

use crate::model::{Flag, GameId, UserId};
use crate::ipc::{LilaOut, LilaIn};

#[derive(StructOpt, Clone)]
struct Opt {
    /// Binding address of Websocket server
    #[structopt(long = "bind", default_value = "127.0.0.1:9664")]
    bind: String,
    /// URI of redis server
    #[structopt(long = "redis", default_value = "redis://127.0.0.1/")]
    redis: String,
    /// URI of mongodb with security collection
    #[structopt(long = "mongodb", default_value = "mongodb://127.0.0.1/")]
    mongodb: String,
    /// Hard limit for maximum number of simultaneous Websocket connections
    #[structopt(long = "max-connections", default_value = "40000")]
    max_connections: usize,
}

/// Messages we send to Websocket clients.
#[derive(Serialize)]
#[serde(tag = "t", content = "d")]
enum SocketIn<'a> {
    #[serde(rename = "fen")]
    Fen {
        id: &'a GameId,
        fen: &'a str,
        lm: &'a str,
    },
    #[serde(rename = "mlat")]
    MoveLatency(u32),
}

impl<'a> SocketIn<'a> {
    fn to_json_string(&self) -> String {
        serde_json::to_string(self).expect("serialize for socket")
    }
}

/// Messages we receive from Websocket clients.
#[derive(Deserialize)]
#[serde(tag = "t")]
enum SocketOut {
    #[serde(rename = "p")]
    Ping { #[allow(unused)] l: Option<u32> },
    #[serde(rename = "notified")]
    Notified,
    #[serde(rename = "startWatching")]
    StartWatching {
        #[serde(deserialize_with = "util::space_separated")]
        d: SmallVec<[GameId; 1]>
    },
    #[serde(rename = "moveLat")]
    MoveLatency { d: bool },
}

/// Session cookie from Play framework.
#[derive(Debug, Deserialize)]
struct SessionCookie {
    #[serde(rename = "sessionId")]
    session_id: String,
}

/// Query string of Websocket requests.
#[derive(Deserialize, Debug)]
struct QueryString {
    flag: Option<Flag>,
}

/// Timeout that's used to close Websockets after some time of inactivity.
const IDLE_TIMEOUT_TOKEN: Token = Token(1);
const IDLE_TIMEOUT_MS: u64 = 15_000;

/// Shared state of this Websocket server.
struct App {
    by_user: RwLock<HashMap::<UserId, Vec<Sender>>>,
    by_game: RwLock<HashMap::<GameId, Vec<Sender>>>,
    by_id: RwLock<HashMap::<SocketId, UserSocket>>,
    watched_games: RwLock<LruCache<GameId, WatchedGame>>,
    flags: [RwLock<HashSet<Sender>>; 2],
    mlat: AtomicU32,
    watching_mlat: RwLock<HashSet<Sender>>,
    redis_sink: channel::Sender<String>,
    sid_sink: channel::Sender<(SocketId, SessionCookie)>,
    broadcaster: OnceCell<Sender>,
    connection_count: AtomicI32, // signed to allow relaxed writes with underflow
}

struct WatchedGame {
    fen: String,
    lm: String,
}

impl App {
    fn new(redis_sink: channel::Sender<String>, sid_sink: channel::Sender<(SocketId, SessionCookie)>) -> App {
        App {
            by_user: RwLock::new(HashMap::new()),
            by_game: RwLock::new(HashMap::new()),
            by_id: RwLock::new(HashMap::new()),
            watched_games: RwLock::new(LruCache::new(5_000)),
            flags: [RwLock::new(HashSet::new()), RwLock::new(HashSet::new())],
            redis_sink,
            sid_sink,
            broadcaster: OnceCell::new(),
            connection_count: AtomicI32::new(0),
            mlat: AtomicU32::new(u32::max_value()),
            watching_mlat: RwLock::new(HashSet::new()),
        }
    }

    fn publish<'a>(&self, msg: LilaIn<'a>) {
        self.redis_sink.send(msg.to_string()).expect("redis sink");
    }

    fn received(&self, msg: LilaOut) {
        match msg {
            LilaOut::TellUsers { users, payload } => {
                let by_user = self.by_user.read();
                for user in users {
                    if let Some(entry) = by_user.get(&user) {
                        for sender in entry {
                            if let Err(err) = sender.send(Message::text(payload.to_string())) {
                                log::error!("failed to tell {}: {:?}", user, err);
                            }
                        }
                    }
                }
            }
            LilaOut::TellAll { payload } => {
                let msg = Message::text(payload.to_string());
                if let Err(err) = self.broadcaster.get().expect("broadcaster").send(msg) {
                    log::error!("failed to broadcast: {:?}", err);
                }
            }
            LilaOut::Move { game, fen, last_uci } => {
                self.watched_games.write().put(game.clone(), WatchedGame {
                    fen: fen.to_owned(),
                    lm: last_uci.to_owned()
                });

                let by_game = self.by_game.read();
                if let Some(entry) = by_game.get(&game) {
                    let msg = Message::text(SocketIn::Fen {
                        id: &game,
                        fen,
                        lm: last_uci,
                    }.to_json_string());

                    for sender in entry {
                        if let Err(err) = sender.send(msg.clone()) {
                            log::error!("failed to send fen: {:?}", err);
                        }
                    }
                }
            }
            LilaOut::MoveLatency(mlat) => {
                // Respond with our stats (connection count).
                self.publish(LilaIn::Connections(
                    max(0, self.connection_count.load(Ordering::Relaxed)) as u32
                ));

                // Update stats.
                self.mlat.store(mlat, Ordering::Relaxed);

                // Update watching clients.
                let msg = SocketIn::MoveLatency(mlat).to_json_string();
                for sender in self.watching_mlat.read().iter() {
                    if let Err(err) = sender.send(msg.clone()) {
                        log::error!("failed to send mlat: {:?}", err);
                    }
                }
            }
            LilaOut::TellFlag { flag, payload } => {
                let watching_flag = self.flags[flag as usize].read();
                let msg = payload.to_string();
                for sender in watching_flag.iter() {
                    if let Err(err) = sender.send(msg.clone()) {
                        log::error!("failed to send to flag ({:?}): {:?}", flag, err);
                    }
                }
            }
        }
    }
}

/// A Websocket client connection.
struct Socket {
    app: &'static App,
    socket_id: SocketId,
    sender: Sender,
    watching: HashSet<GameId>,
    flag: Option<Flag>,
    idle_timeout: Option<Timeout>,
}

/// Uniquely identifies a socket connection over the entire runtime of the
/// application.
#[derive(Hash, Eq, PartialEq, Copy, Clone)]
struct SocketId(pub u64);

enum SocketAuth {
    Requested,
    Authenticated(UserId),
    Anonymous,
}

struct UserSocket {
    app: &'static App,
    sender: Sender,
    auth: SocketAuth,
    pending_notified: bool,
}

impl UserSocket {
    fn set_user(&mut self, maybe_uid: Option<UserId>) {
        // Connected.
        let auth = match maybe_uid {
            Some(uid) => {
                self.app.by_user.write()
                    .entry(uid.clone())
                    .and_modify(|v| v.push(self.sender.clone()))
                    .or_insert_with(|| {
                        log::debug!("first open: {}", uid);
                        self.app.publish(LilaIn::Connect(&uid));
                        vec![self.sender.clone()]
                    });

                SocketAuth::Authenticated(uid)
            },
            None => SocketAuth::Anonymous,
        };

        // Disconnected.
        match mem::replace(&mut self.auth, auth) {
            SocketAuth::Authenticated(uid) => {
                let mut by_user = self.app.by_user.write();
                let entry = by_user.get_mut(&uid).expect("uid in by_user");
                let idx = entry.iter().position(|s| s.token() == self.sender.token()).expect("sender in by_user entry");
                entry.swap_remove(idx);

                // Last remaining connection closed.
                if entry.is_empty() {
                    by_user.remove(&uid);
                    log::debug!("last close: {}", uid);
                    self.app.publish(LilaIn::Disconnect(&uid));
                }
            },
            _ => (),
        }

        if self.pending_notified {
            self.on_notified();
        }
    }

    fn on_ping(&self, lag: u32) {
        if let SocketAuth::Authenticated(ref uid) = self.auth {
            // TODO: Wait for deploy of
            // https://github.com/ornicar/lila/commit/3c2854b082df974141ac5818d4d02a443195d8ce
            //self.app.publish(LilaIn::Lag(uid, lag));
        }
    }

    fn on_notified(&mut self) {
        self.pending_notified = false;
        match &self.auth {
            SocketAuth::Requested => self.pending_notified = true,
            SocketAuth::Authenticated(uid) => self.app.publish(LilaIn::Notified(uid)),
            SocketAuth::Anonymous => log::warn!("anon notified"),
        }
    }
}

impl Handler for Socket {
    fn on_open(&mut self, handshake: Handshake) -> ws::Result<()> {
        // Update connection count.
        self.app.connection_count.fetch_add(1, Ordering::Relaxed);

        // Parse session cookie.
        let maybe_cookie = handshake.request.header("cookie")
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
            });

        // Update by_id.
        self.app.by_id.write().insert(self.socket_id, UserSocket {
            app: self.app,
            auth: if maybe_cookie.is_some() { SocketAuth::Requested } else { SocketAuth::Anonymous },
            pending_notified: false,
            sender: self.sender.clone(),
        });

        // Request authentication.
        if let Some(cookie) = maybe_cookie {
            self.app.sid_sink.send((self.socket_id, cookie)).expect("auth request");
        }

        // Subscribe to flag.
        let path = handshake.request.resource();
        if let Some(qs_idx) = path.find('?') {
            let qs = &path[qs_idx..];
            match serde_urlencoded::from_str::<QueryString>(qs) {
                Ok(QueryString { flag: Some(flag) }) => {
                    self.app.flags[flag as usize].write().insert(self.sender.clone());
                    self.flag = Some(flag);
                },
                Ok(_) => (),
                Err(err) => log::warn!("invalid query string ({:?}): {}", err, qs),
            }
        }

        // Start idle timeout.
        self.sender.timeout(IDLE_TIMEOUT_MS, IDLE_TIMEOUT_TOKEN)
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

        // Update by_id.
        let mut user_socket = self.app.by_id.write().remove(&self.socket_id).expect("user socket");
        user_socket.set_user(None);

        // Update by_game.
        let mut by_game = self.app.by_game.write();
        let our_token = self.sender.token();
        for game in self.watching.drain() {
            let watchers = by_game.get_mut(&game).expect("game in by_game");
            let idx = watchers.iter().position(|s| s.token() == our_token).expect("sender in watchers");
            watchers.swap_remove(idx);
            if watchers.is_empty() {
                by_game.remove(&game);
                log::debug!("no more watchers for {:?}", game);
            }
        }

        // Unsubscribe from flag.
        if let Some(flag) = self.flag.take() {
            self.app.flags[flag as usize].write().remove(&self.sender);
        }
    }

    fn on_message(&mut self, msg: Message) -> ws::Result<()> {
        self.sender.timeout(IDLE_TIMEOUT_MS, IDLE_TIMEOUT_TOKEN)?;

        // Fast path for ping.
        let msg = msg.as_text()?;
        if msg == "null" {
            return self.sender.send(Message::text("0"));
        }

        // Limit message size.
        if msg.len() > 512 {
            log::warn!("very long message ({} bytes): {}", msg.len(), msg);
            return self.sender.close(CloseCode::Size);
        } else if msg.len() > 256 {
            log::info!("long message ({} bytes): {}", msg.len(), msg);
        }

        match serde_json::from_str(msg) {
            Ok(SocketOut::Ping { l }) => {
                if let Some(lag) = l {
                    self.app.by_id.read().get(&self.socket_id).expect("user socket").on_ping(lag);
                }
                self.sender.send(Message::text("0"))
            }
            Ok(SocketOut::Notified) => {
                let mut write_guard = self.app.by_id.write();
                write_guard.get_mut(&self.socket_id)
                    .expect("user socket")
                    .on_notified();
                Ok(())
            }
            Ok(SocketOut::StartWatching { d }) => {
                for game in d {
                    if self.watching.insert(game.clone()) {
                        if self.watching.len() > 20 {
                            log::info!("client is watching many games: {}", self.watching.len());
                        }

                        // If cached, send current game state immediately.
                        let was_cached = match self.app.watched_games.read().peek(&game) {
                            Some(state) => {
                                self.sender.send(SocketIn::Fen {
                                    id: &game,
                                    fen: &state.fen,
                                    lm: &state.lm,
                                }.to_json_string())?;
                                true
                            },
                            None => false,
                        };

                        // Subscribe to updates.
                        self.app.by_game.write()
                            .entry(game.clone())
                            .and_modify(|v| {
                                v.push(self.sender.clone());
                                log::debug!("also watching {:?} ({} watchers)", game, v.len());
                            })
                            .or_insert_with(|| {
                                if was_cached {
                                    log::debug!("a watcher returned to {:?}", game);
                                } else {
                                    log::debug!("start watching: {:?}", game);
                                    self.app.publish(LilaIn::Watch(&game));
                                }
                                vec![self.sender.clone()]
                            });
                    }
                }
                Ok(())
            },
            Ok(SocketOut::MoveLatency { d }) => {
                let mut watching_mlat = self.app.watching_mlat.write();
                if d {
                    if watching_mlat.insert(self.sender.clone()) {
                        self.sender.send(SocketIn::MoveLatency(
                            self.app.mlat.load(Ordering::Relaxed)
                        ).to_json_string())?;
                    }
                } else {
                    watching_mlat.remove(&self.sender);
                }
                Ok(())
            },
            Err(err) => {
                log::warn!("protocol violation of client ({:?}): {}", err, msg);
                self.sender.close(CloseCode::Protocol)
            }
        }
    }

    fn on_new_timeout(&mut self, event: Token, timeout: Timeout) -> ws::Result<()> {
        assert_eq!(event, IDLE_TIMEOUT_TOKEN);
        if let Some(old_timeout) = self.idle_timeout.take() {
            self.sender.cancel(old_timeout)?;
        }
        self.idle_timeout = Some(timeout);
        Ok(())
    }

    fn on_timeout(&mut self, event: Token) -> ws::Result<()> {
        assert_eq!(event, IDLE_TIMEOUT_TOKEN);
        log::info!("closing socket due to timeout");
        self.sender.close(CloseCode::Away)
    }
}

fn main() {
    env_logger::init();

    crossbeam::scope(|s| {
        let opt = Opt::from_args();

        let (redis_sink, redis_recv) = channel::unbounded();
        let (sid_sink, sid_recv) = channel::unbounded();
        let app: &'static App = Box::leak(Box::new(App::new(redis_sink, sid_sink)));

        // Clear connections and subscriptions from previous process.
        app.publish(LilaIn::DisconnectAll);

        // Thread for outgoing messages to lila.
        let opt_inner = opt.clone();
        s.spawn(move |_| {
            let redis = redis::Client::open(opt_inner.redis.as_str())
                .expect("redis open for publish")
                .get_connection()
                .expect("redis connection for publish");

            loop {
                let msg: String = redis_recv.recv().expect("redis recv");
                log::trace!("site-in: {}", msg);
                let ret: u32 = redis.publish("site-in", msg).expect("publish site-in");
                if ret == 0 {
                    log::error!("lila missed as message");
                }
            }
        });

        // Thread for session id lookups.
        let opt_inner = opt.clone();
        s.spawn(move |_| {
            let session_store = mongodb::Client::with_uri(opt_inner.mongodb.as_str())
                .expect("mongodb connect")
                .db("lichess")
                .collection("security");

            loop {
                let (socket_id, cookie) = sid_recv.recv().expect("socket id recv");

                let query = doc! { "_id": &cookie.session_id, "up": true, };
                let mut opts = FindOptions::new();
                opts.projection = Some(doc! { "user": true });

                let maybe_uid = match session_store.find_one(Some(query), Some(opts)) {
                    Ok(Some(doc)) => doc.get_str("user").ok().and_then(|s| UserId::new(s).ok()),
                    Ok(None) => {
                        log::info!("session store does not have sid: {}", cookie.session_id);
                        None
                    },
                    Err(err) => {
                        log::error!("session store query failed: {:?}", err);
                        None
                    },
                };

                let mut write_guard = app.by_id.write();
                if let Some(user_socket) = write_guard.get_mut(&socket_id) {
                    user_socket.set_user(maybe_uid);
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
                let msg = incoming.get_message()
                    .expect("get message")
                    .get_payload::<String>()
                    .expect("get payload");

                match LilaOut::parse(&msg) {
                    Ok(msg) => app.received(msg),
                    Err(_) => log::error!("invalid message from lila: {}", msg),
                }
            }
        });

        // Start websocket server.
        let mut settings = ws::Settings::default();
        settings.max_connections = opt.max_connections;
        settings.queue_size = 10;
        settings.tcp_nodelay = true;
        settings.in_buffer_grow = false;

        let mut socket_id = 0;

        let server = ws::Builder::new()
            .with_settings(settings)
            .build(move |sender| {
                socket_id += 1;
                Socket {
                    app,
                    sender,
                    socket_id: SocketId(socket_id),
                    flag: None,
                    watching: HashSet::new(),
                    idle_timeout: None
                }
            })
            .expect("valid settings");

        app.broadcaster.set(server.broadcaster()).expect("set broadcaster");

        server.listen(&opt.bind).expect("ws listen");
    }).expect("scope");
}
