use async_std::channel::{unbounded, Sender, Receiver, Recv};
use async_std::io::{Error, ErrorKind};
use async_std::net::{TcpListener, TcpStream};
use async_std::prelude::*;
use async_std::sync::*;
use async_std::task::spawn;
use bincode::Options;
use futures::stream::StreamExt;
use judge_protocol::constants::*;
use judge_protocol::handshake::*;
use judge_protocol::judge::*;
use judge_protocol::packet::*;
use judge_protocol::security::*;
use k256::ecdh::{EphemeralSecret, SharedSecret};
use k256::sha2::digest::typenum::private::IsEqualPrivate;
use k256::PublicKey;
use log::*;
use rand::thread_rng;
use sha3::{Digest, Sha3_256};
use std::collections::HashMap;
use std::pin::Pin;
use uuid::Uuid;

use crate::config::Config;
use crate::judge::{PrioirityWeight, RequestJudge, TestCaseManager};
use crate::scheduler::{
    by_deadline::ByDeadlineWeighted, SchedulerMessage, SchedulerResult, SchedulerWeighted,
};

pub struct State {
    pub cfg: Arc<Mutex<Config>>,
    host_pass: Arc<Mutex<Vec<u8>>>,
    count: Mutex<u32>,
    key: Arc<EphemeralSecret>,
    pubkey: Arc<Mutex<Vec<PublicKey>>>,
    shared: Arc<Mutex<Vec<SharedSecret>>>,
    judges: Arc<Mutex<HashMap<Uuid, RequestJudge>>>,
    testman: Arc<Mutex<Vec<Option<Box<TestCaseManager>>>>>,
    peers: Arc<Mutex<Vec<Sender<Vec<u8>>>>>,
    scheduler: Arc<Mutex<ByDeadlineWeighted>>,
}

pub async fn serve(cfg: Config) {
    let mut hasher = Sha3_256::new();
    hasher.update(cfg.host_pass.as_bytes());
    let key = EphemeralSecret::random(thread_rng());
    let (scheduler_tx, scheduler_rx) = unbounded();
    let state = Arc::new(Mutex::new(State {
        cfg: Arc::new(Mutex::new(cfg.clone())),
        host_pass: Arc::new(Mutex::new(hasher.finalize().to_vec())),
        count: Mutex::new(0),
        key: Arc::new(key),
        shared: Arc::new(Mutex::new(vec![])),
        judges: Arc::new(Mutex::new(HashMap::new())),
        pubkey: Arc::new(Mutex::new(vec![])),
        peers: Arc::new(Mutex::new(vec![])),
        testman: Arc::new(Mutex::new(vec![])),
        scheduler: Arc::new(Mutex::new(ByDeadlineWeighted::new(Arc::new(Mutex::new(
            scheduler_tx,
        ))))),
    }));
    let listener = TcpListener::bind(cfg.host)
        .await
        .expect(&format!("Cannot bind {:?}", cfg.host));
    listener
        .incoming()
        .for_each_concurrent(None, |stream| async {
            let stream = stream.unwrap();
            let state_mutex = Arc::clone(&state);
            spawn(async move { (state_mutex.lock().await).handle_connection(stream).await });
            //drop(state_mutex);
        })
        .await;
    /*spawn(async move {
        loop {
            if let Ok(msg) = scheduler_rx.try_recv() {
                match msg {
                    SchedulerMessage::Send(uuid, node_id) => {
                        let state_mutex = Arc::clone(&state_mutex2);
                        state_mutex
                            .lock()
                            .await
                            .handle_judge_send(uuid, node_id)
                            .await
                            .ok();
                    }
                    _ => {}
                }
            }
        }
    });*/
}

pub async fn serve_stream(stream: Arc<TcpStream>, packet_rx: &mut Receiver<Vec<u8>>) {
    let mut stream = &*stream;
    loop {
        if let Ok(msg) = packet_rx.try_recv() {
            stream.write_all(&msg).await;
            stream.flush().await;
            debug!("{:?}", msg);
        }
    }
}

impl State {
    pub async fn handle_connection(&mut self, mut stream: TcpStream) -> async_std::io::Result<()> {
        info!("Established connection from {:?}", stream.peer_addr());
        let stream = Arc::new(stream);
        let packet = Packet::from_stream(Arc::clone(&stream)).await?;
        self.handle_command(Arc::clone(&stream), packet).await
    }

    pub async fn req_judge(&mut self, judge: RequestJudge) -> SchedulerResult<()> {
        let estimated_time = (judge.test_size as u64) * judge.time_limit;
        let _ = self
            .scheduler
            .lock()
            .await
            .push(judge.uuid, estimated_time, judge.judge_priority as u64)
            .await?;
        self.judges.lock().await.insert(judge.uuid, judge);
        debug!("야옹");
        Ok(())
    }

    pub async fn handle_judge_send(
        &mut self,
        uuid: Uuid,
        node_id: u32,
    ) -> async_std::io::Result<()> {
        let mut sender = self.peers.lock().await[node_id as usize].clone();
        let judges = self.judges.lock().await;
        let judge = judges.get(&uuid).unwrap();
        let shared = &(*self.shared.lock().await)[node_id as usize];
        let key = expand_key(&shared);
        let body = JudgeRequestBody {
            uuid: judge.uuid,
            main_lang: judge.main_lang_uuid,
            checker_lang: judge.checker_lang_uuid,
            checker_code: EncMessage::generate(&key, &judge.checker),
            main_code: EncMessage::generate(&key, &judge.main),
            mem_limit: judge.mem_limit,
            time_limit: judge.time_limit,
        };
        self.testman.lock().await[node_id as usize] =
            Some(Box::new(TestCaseManager::from(&judge.stdin, &judge.stdout)));
        let packet = Packet::make_packet(
            Command::GetJudge,
            bincode::DefaultOptions::new()
                .with_big_endian()
                .with_fixint_encoding()
                .serialize(&body)
                .unwrap(),
        );
        packet
            .send_with_sender(&mut sender);
        Ok(())
    }

    async fn unlock_slave(&mut self, node_id: u32) {
        // TODO
        debug!("[node#{}] unlocked!", node_id);
        self.scheduler
            .lock()
            .await
            .touch(node_id as usize)
            .await
            .ok();
        drop(self.testman.lock().await.iter().nth(node_id as usize));
    }

    async fn send_testcase(
        &mut self,
        stream: Arc<TcpStream>,
        judge_uuid: Uuid,
        node_id: u32,
    ) -> bool {
        let test_uuid = self.testman.lock().await[node_id as usize]
            .as_mut()
            .unwrap()
            .next();
        if test_uuid.is_nil() {
            self.unlock_slave(node_id).await;
            false
        } else {
            let testman = self.testman.lock().await;
            let key = expand_key(&self.shared.lock().await[node_id as usize]);
            let (stdin, stdout) = testman[node_id as usize].as_ref().unwrap().get(test_uuid);
            let body = TestCaseUpdateBody {
                uuid: judge_uuid,
                test_uuid,
                stdin: EncMessage::generate(&key, stdin),
                stdout: EncMessage::generate(&key, stdout),
            };
            let packet = Packet::make_packet(
                Command::TestCaseUpdate,
                bincode::DefaultOptions::new()
                    .with_big_endian()
                    .with_fixint_encoding()
                    .serialize::<TestCaseUpdateBody>(&body)
                    .unwrap(),
            );
            packet.send(Arc::clone(&stream)).await.ok();
            true
        }
    }

    async fn handle_command(
        &mut self,
        stream: Arc<TcpStream>,
        packet: Packet,
    ) -> async_std::io::Result<()> {
        match packet.heady.header.command {
            Command::GetJudgeStateUpdate => {
                if let Ok(body) = bincode::DefaultOptions::new()
                    .with_big_endian()
                    .with_fixint_encoding()
                    .deserialize::<BodyAfterHandshake<JudgeResponseBody>>(&packet.heady.body)
                {
                    match body.req.result {
                        JudgeState::DoCompile => {
                            debug!(
                                "[node#{}] (Judge: {}) started compile codes",
                                body.node_id, body.req.uuid
                            );
                            Ok(())
                        }
                        JudgeState::CompleteCompile(stdout) => {
                            debug!(
                                "[node#{}] (Judge: {}) main code compile stdout: {}",
                                body.node_id, body.req.uuid, stdout
                            );
                            let _ = self
                                .send_testcase(Arc::clone(&stream), body.req.uuid, body.node_id)
                                .await;
                            Ok(())
                        }
                        JudgeState::CompileError(stderr) => {
                            debug!("[node#{}] (Judge: {}) master has received report CE of main code. stderr: {}", body.node_id, body.req.uuid, stderr);
                            self.unlock_slave(body.node_id).await;
                            Ok(())
                        }
                        JudgeState::Accepted(test_uuid, time, mem) => {
                            debug!("[node#{}] (Judge: {}) master has recived report AC of main code. time: {}, mem: {}", body.node_id, body.req.uuid, time, mem);
                            let _ = self
                                .send_testcase(Arc::clone(&stream), body.req.uuid, body.node_id)
                                .await;
                            Ok(())
                        }
                        _ => {
                            self.unlock_slave(body.node_id).await;
                            Ok(())
                        }
                    }
                } else {
                    Err(Error::new(ErrorKind::InvalidData, "Invalid packet"))
                }
            }
            Command::Handshake => {
                if let Ok(handshake_req) = bincode::DefaultOptions::new()
                    .with_big_endian()
                    .with_fixint_encoding()
                    .deserialize::<HandshakeRequest>(&packet.heady.body)
                {
                    if handshake_req.pass == *self.host_pass.lock().await {
                        debug!("Handshake");
                        self.shared
                            .lock()
                            .await
                            .push(self.key.diffie_hellman(&handshake_req.client_pubkey));
                        self.pubkey.lock().await.push(handshake_req.client_pubkey);
                        self.testman.lock().await.push(None);
                        let handshake_res = HandshakeResponse {
                            result: HandshakeResult::Success,
                            node_id: Some((*self.count.lock().await) + 1),
                            server_pubkey: Some(self.key.public_key().clone()),
                        };
                        let req_packet = Packet::make_packet(
                            Command::Handshake,
                            bincode::DefaultOptions::new()
                                .with_big_endian()
                                .with_fixint_encoding()
                                .serialize(&handshake_res)
                                .unwrap(),
                        );
                        (*self.count.lock().await) += 1;
                        self.scheduler.lock().await.register();
                        let (packet_tx, mut packet_rx) = unbounded();
                        spawn(async move { serve_stream(stream, &mut packet_rx).await });
                        req_packet.send_with_sender(&mut packet_tx.clone()).await;
                        self.peers.lock().await.push(packet_tx);
                        Ok(())
                    } else {
                        let handshake_res = HandshakeResponse {
                            result: HandshakeResult::PasswordNotMatched,
                            node_id: None,
                            server_pubkey: None,
                        };
                        let req_packet = Packet::make_packet(
                            Command::Handshake,
                            bincode::DefaultOptions::new()
                                .with_big_endian()
                                .with_fixint_encoding()
                                .serialize(&handshake_res)
                                .unwrap(),
                        );
                        req_packet.send(Arc::clone(&stream)).await
                    }
                } else {
                    Err(Error::new(ErrorKind::InvalidData, "Invalid packet"))
                }
            }
            Command::VerifyToken => {
                if let Ok(body) = bincode::DefaultOptions::new()
                    .with_big_endian()
                    .with_fixint_encoding()
                    .deserialize::<BodyAfterHandshake<()>>(&packet.heady.body)
                {
                    let client_pubkey = body.client_pubkey;
                    let ret = (*self.count.lock().await <= body.node_id)
                        || (self.pubkey.lock().await[body.node_id as usize] == client_pubkey);
                    let req_packet = Packet::make_packet(
                        Command::ReqVerifyToken,
                        bincode::DefaultOptions::new()
                            .with_big_endian()
                            .with_fixint_encoding()
                            .serialize(&ret)
                            .unwrap(),
                    );
                    req_packet.send(Arc::clone(&stream)).await
                } else {
                    Err(Error::new(ErrorKind::InvalidData, "Invalid packet"))
                }
            }
            _ => Ok(()),
        }
    }
}
