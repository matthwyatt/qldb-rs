use async_channel::{bounded, unbounded, Sender, TrySendError};
use async_compat::CompatExt;
use async_executor::LocalExecutor;
use async_io::Timer;
use async_lock::Mutex;
use eyre::WrapErr;
use futures_lite::future;
use rusoto_core::RusotoError;
use rusoto_qldb_session::{
    EndSessionRequest, QldbSession, QldbSessionClient, SendCommandRequest, StartSessionRequest,
};
use std::collections::VecDeque;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicU16, Ordering::Relaxed},
    Arc,
};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct InnerSession {
    created_on_instant: Instant,
    session_id: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    inner: Arc<InnerSession>,
}

impl Session {
    pub fn new(session_id: String) -> Session {
        Session {
            inner: Arc::new(InnerSession {
                created_on_instant: Instant::now(),
                session_id,
            }),
        }
    }

    pub fn get_session_id(&self) -> &str {
        &self.inner.session_id
    }

    pub fn is_valid(&self) -> bool {
        self.inner.created_on_instant.elapsed().as_secs() < 10 * 60
    }
}

#[derive(Debug)]
pub enum PoolCommand {
    Request(Sender<Session>),
    Return(Session),
}

#[derive(Debug, Clone)]
pub struct SessionPool {
    sender: Sender<PoolCommand>,
    closer: Arc<Mutex<Option<PoolEndFuture>>>,
}

impl SessionPool {
    pub fn new(
        qldb_client: Arc<QldbSessionClient>,
        ledger_name: &str,
        max_sessions: u16,
    ) -> SessionPool {
        let (sender, receiver) = unbounded::<PoolCommand>();
        let ledger_name = ledger_name.to_owned();

        let closer = PoolEndFuture::new();
        let closer_executor = PoolEndFuture::new();

        std::thread::spawn(move || {
            let executor = LocalExecutor::new();
            let sessions = Rc::new(Mutex::new(VecDeque::<Session>::new()));
            let session_requests = Rc::new(Mutex::new(VecDeque::<Sender<Session>>::new()));
            let active_session_count = Rc::new(AtomicU16::new(0));
            let (session_create_request, session_create_recv) = unbounded::<()>();

            {
                let sessions = sessions.clone();
                let session_requests = session_requests.clone();
                let active_session_count = active_session_count.clone();
                let qldb_client = qldb_client.clone();
                let session_create_request = session_create_request.clone();

                executor
                    .spawn(async move {
                        while let Ok(message) = receiver.recv().await {
                            match message {
                                PoolCommand::Return(session) => {
                                    if !session.is_valid() {
                                        close_session(
                                            &qldb_client,
                                            &session,
                                            &active_session_count,
                                        )
                                        .await;

                                        continue;
                                    }

                                    sessions.lock().await.push_front(session);

                                    try_send_session_to_session_requesters(
                                        &sessions,
                                        &session_requests,
                                    )
                                    .await;
                                }
                                PoolCommand::Request(sender) => loop {
                                    let session = sessions.lock().await.pop_back();

                                    match session {
                                        Some(session) => {
                                            if !session.is_valid() {
                                                close_session(
                                                    &qldb_client,
                                                    &session,
                                                    &active_session_count,
                                                )
                                                .await;

                                                continue;
                                            }

                                            try_send_session(&sender, session, &sessions).await;
                                        }
                                        None => {
                                            session_requests.lock().await.push_front(sender);
                                            let _ = session_create_request.send(()).await;
                                        }
                                    }

                                    break;
                                },
                            }
                        }
                    })
                    .detach();
            }

            {
                let sessions = sessions;
                let session_requests = session_requests;
                let active_session_count = active_session_count;

                executor
                    .spawn(async move {
                        while session_create_recv.recv().await.is_ok() {
                            if active_session_count.load(Relaxed) >= max_sessions {
                                continue;
                            }

                            match create_session(&qldb_client, &ledger_name).await {
                                Ok(session) => {
                                    add_session(&active_session_count, &sessions, session).await;

                                    try_send_session_to_session_requesters(
                                        &sessions,
                                        &session_requests,
                                    )
                                    .await;

                                    if active_session_count.load(Relaxed) < max_sessions
                                        && !session_requests.lock().await.is_empty()
                                    {
                                        let _ = session_create_request.send(()).await;
                                    }
                                }
                                Err(_) => {
                                    Timer::after(Duration::from_millis(100)).await;

                                    let _ = session_create_request.send(()).await;
                                }
                            };
                        }
                    })
                    .detach();
            }

            future::block_on(executor.run(closer_executor));
        });

        SessionPool {
            sender,
            closer: Arc::new(Mutex::new(Some(closer))),
        }
    }

    pub async fn close(&self) {
        if let Some(closer) = self.closer.lock().await.take() {
            closer.close();
        }
    }

    pub async fn get(&self) -> eyre::Result<Session> {
        let (sender, receiver) = bounded::<Session>(1);

        self.sender
            .try_send(PoolCommand::Request(sender))
            .wrap_err("Session pool closed")?;

        let session = receiver.recv().await.wrap_err("Session pool closed")?;

        Ok(session)
    }

    pub fn give_back(&self, session: Session) {
        let _ = self.sender.try_send(PoolCommand::Return(session));
    }
}

async fn create_session(
    qldb_client: &Arc<QldbSessionClient>,
    ledger_name: &str,
) -> Result<Session, GetSessionError> {
    let mut tries: u32 = 0;

    let session = loop {
        tries = tries.saturating_add(1);

        match qldb_request_session(qldb_client, ledger_name).await {
            Ok(session) => break Ok(session),
            Err(error) if tries > 10 => break Err(error),
            Err(GetSessionError::Recoverable(_)) => {
                Timer::after(Duration::from_millis(
                    tries.saturating_mul(tries).saturating_mul(75).into(),
                ))
                .await;
            }
            Err(GetSessionError::Unrecoverable(error)) => {
                break Err(GetSessionError::Unrecoverable(error))
            }
        }
    }?;

    Ok(Session::new(session))
}

#[derive(Debug, thiserror::Error)]
enum GetSessionError {
    #[error("The QLDB command returned an error")]
    Unrecoverable(eyre::Report),
    #[error("The QLDB command returned an error")]
    Recoverable(eyre::Report),
}

async fn qldb_request_session(
    qldb_client: &QldbSessionClient,
    ledger_name: &str,
) -> Result<String, GetSessionError> {
    match qldb_client
        .send_command(SendCommandRequest {
            start_session: Some(StartSessionRequest {
                ledger_name: ledger_name.to_string(),
            }),
            ..Default::default()
        })
        .compat()
        .await
    {
        Ok(response) => match response.start_session {
            Some(session) => match session.session_token {
                Some(token) => Ok(token),
                None => Err(GetSessionError::Unrecoverable(eyre::eyre!(
                    "No session present on QLDB response"
                ))),
            },
            None => Err(GetSessionError::Unrecoverable(eyre::eyre!(
                "Empty session on QLDB response"
            ))),
        },
        Err(err) => match err {
            RusotoError::Credentials(_) => Err(GetSessionError::Unrecoverable(eyre::eyre!(err))),
            _ => Err(GetSessionError::Recoverable(eyre::eyre!(err))),
        },
    }
}

async fn close_session(
    qldb_client: &Arc<QldbSessionClient>,
    session: &Session,
    active_session_count: &Rc<AtomicU16>,
) {
    let mut tries: u32 = 0;

    loop {
        tries = tries.saturating_add(1);

        match qldb_close_session(qldb_client, session).await {
            Ok(_) => break,
            Err(_) if tries > 10 => break,
            Err(_) => {
                Timer::after(Duration::from_millis(
                    tries.saturating_mul(tries).saturating_mul(75).into(),
                ))
                .await;
            }
        }
    }

    active_session_count.store(
        active_session_count.load(Relaxed).saturating_sub(1),
        Relaxed,
    );
}

async fn qldb_close_session(
    qldb_client: &QldbSessionClient,
    session: &Session,
) -> Result<(), eyre::Report> {
    qldb_client
        .send_command(SendCommandRequest {
            session_token: Some(session.get_session_id().to_string()),
            end_session: Some(EndSessionRequest {}),
            ..Default::default()
        })
        .await?;

    Ok(())
}

fn get_session_from_send_err(error: TrySendError<Session>) -> Session {
    match error {
        TrySendError::Full(session) => session,
        TrySendError::Closed(session) => session,
    }
}

async fn try_send_session_to_session_requesters(
    sessions: &Rc<Mutex<VecDeque<Session>>>,
    session_requests: &Rc<Mutex<VecDeque<Sender<Session>>>>,
) {
    let session = loop {
        let session = sessions.lock().await.pop_back();

        match session {
            None => break None,
            Some(session) => {
                if let Some(sender) = session_requests.lock().await.pop_back() {
                    if let Err(error) = sender.try_send(session) {
                        let session = get_session_from_send_err(error);

                        sessions.lock().await.push_front(session);

                        continue;
                    }
                } else {
                    break Some(session);
                }
            }
        }
    };

    if let Some(session) = session {
        sessions.lock().await.push_front(session);
    }
}

async fn try_send_session(
    sender: &Sender<Session>,
    session: Session,
    sessions: &Rc<Mutex<VecDeque<Session>>>,
) {
    if let Err(error) = sender.try_send(session) {
        let session = get_session_from_send_err(error);

        sessions.lock().await.push_front(session);
    }
}

async fn add_session(
    active_session_count: &Rc<AtomicU16>,
    sessions: &Rc<Mutex<VecDeque<Session>>>,
    session: Session,
) {
    active_session_count.store(
        active_session_count.load(Relaxed).saturating_add(1),
        Relaxed,
    );
    sessions.lock().await.push_front(session);
}

#[derive(Debug, Clone)]
struct PoolEndFuture {
    waker: Option<Waker>,
    ready: bool,
}

impl PoolEndFuture {
    fn new() -> PoolEndFuture {
        PoolEndFuture {
            waker: None,
            ready: false,
        }
    }

    fn close(mut self) {
        self.ready = true;

        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

impl std::future::Future for PoolEndFuture {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.ready {
            Poll::Ready(())
        } else {
            self.waker = Some(context.waker().clone());
            Poll::Pending
        }
    }
}
