//! PAM authentication and a non-blocking worker boundary.

use super::secret::SecretBuffer;
use pam_client2::{Context, ConversationHandler, ErrorCode, Flag};
use std::ffi::{CStr, CString};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthStage {
    Initialize,
    Authenticate,
    Account,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthOutcome {
    Success,
    Denied { stage: AuthStage, code: ErrorCode },
    Error { stage: AuthStage, code: ErrorCode },
}

pub trait AuthProvider: Send + 'static {
    fn authenticate(&mut self, secret: SecretBuffer) -> AuthOutcome;
}

pub struct PamProvider {
    service: String,
    username: String,
}

impl PamProvider {
    #[must_use]
    pub fn new(service: impl Into<String>, username: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            username: username.into(),
        }
    }
}

impl AuthProvider for PamProvider {
    fn authenticate(&mut self, secret: SecretBuffer) -> AuthOutcome {
        let conversation = PasswordConversation::new(self.username.clone(), secret);
        let mut context = match Context::new(&self.service, Some(&self.username), conversation) {
            Ok(context) => context,
            Err(error) => {
                return AuthOutcome::Error {
                    stage: AuthStage::Initialize,
                    code: error.code(),
                };
            }
        };

        if let Err(error) = context.authenticate(Flag::DISALLOW_NULL_AUTHTOK) {
            return classify_error(AuthStage::Authenticate, error.code());
        }
        if let Err(error) = context.acct_mgmt(Flag::DISALLOW_NULL_AUTHTOK) {
            return classify_error(AuthStage::Account, error.code());
        }
        AuthOutcome::Success
    }
}

fn classify_error(stage: AuthStage, code: ErrorCode) -> AuthOutcome {
    if matches!(
        code,
        ErrorCode::AUTH_ERR
            | ErrorCode::USER_UNKNOWN
            | ErrorCode::MAXTRIES
            | ErrorCode::PERM_DENIED
            | ErrorCode::ACCT_EXPIRED
            | ErrorCode::NEW_AUTHTOK_REQD
            | ErrorCode::AUTHTOK_EXPIRED
    ) {
        AuthOutcome::Denied { stage, code }
    } else {
        AuthOutcome::Error { stage, code }
    }
}

struct PasswordConversation {
    username: CString,
    secret: SecretBuffer,
    secret_was_requested: bool,
}

impl PasswordConversation {
    fn new(username: String, secret: SecretBuffer) -> Self {
        let username = CString::new(username).unwrap_or_default();
        Self {
            username,
            secret,
            secret_was_requested: false,
        }
    }
}

impl ConversationHandler for PasswordConversation {
    fn prompt_echo_on(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
        Ok(self.username.clone())
    }

    fn prompt_echo_off(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
        // Reusing one password for an unexpected second hidden prompt could
        // authenticate a different factor incorrectly. Reject it until the UI
        // supports an interactive multi-prompt PAM conversation.
        if self.secret_was_requested {
            return Err(ErrorCode::CONV_ERR);
        }
        self.secret_was_requested = true;
        Ok(self.secret.to_c_string())
    }

    fn text_info(&mut self, _message: &CStr) {}

    fn error_msg(&mut self, _message: &CStr) {}
}

struct AuthRequest {
    attempt: u64,
    secret: SecretBuffer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthResponse {
    pub attempt: u64,
    pub outcome: AuthOutcome,
}

#[derive(Debug)]
pub enum SubmitError {
    Busy(SecretBuffer),
    Stopped(SecretBuffer),
}

pub struct AuthWorker {
    requests: Option<SyncSender<AuthRequest>>,
    responses: Receiver<AuthResponse>,
    next_attempt: u64,
    in_flight: bool,
}

impl AuthWorker {
    pub fn spawn<P: AuthProvider>(mut provider: P) -> std::io::Result<Self> {
        let (request_tx, request_rx) = mpsc::sync_channel::<AuthRequest>(1);
        let (response_tx, response_rx) = mpsc::sync_channel::<AuthResponse>(1);

        thread::Builder::new()
            .name("helium-locker-auth".into())
            .spawn(move || {
                while let Ok(request) = request_rx.recv() {
                    let outcome = provider.authenticate(request.secret);
                    if response_tx
                        .send(AuthResponse {
                            attempt: request.attempt,
                            outcome,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })?;

        Ok(Self {
            requests: Some(request_tx),
            responses: response_rx,
            next_attempt: 1,
            in_flight: false,
        })
    }

    pub fn submit(&mut self, secret: SecretBuffer) -> Result<u64, SubmitError> {
        if self.in_flight {
            return Err(SubmitError::Busy(secret));
        }
        let attempt = self.next_attempt;
        let request = AuthRequest { attempt, secret };
        let result = match self.requests.as_ref() {
            Some(sender) => sender.try_send(request),
            None => return Err(SubmitError::Stopped(request.secret)),
        };

        match result {
            Ok(()) => {
                self.next_attempt = self.next_attempt.wrapping_add(1).max(1);
                self.in_flight = true;
                Ok(attempt)
            }
            Err(TrySendError::Full(request)) => Err(SubmitError::Busy(request.secret)),
            Err(TrySendError::Disconnected(request)) => Err(SubmitError::Stopped(request.secret)),
        }
    }

    pub fn try_response(&mut self) -> Result<Option<AuthResponse>, WorkerStopped> {
        match self.responses.try_recv() {
            Ok(response) => {
                self.in_flight = false;
                Ok(Some(response))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.in_flight = false;
                Err(WorkerStopped)
            }
        }
    }
}

impl Drop for AuthWorker {
    fn drop(&mut self) {
        // Do not join: a PAM module may be blocked in external I/O. Dropping
        // the sender lets an idle worker exit, while process termination is
        // still guaranteed not to request an unlock.
        self.requests.take();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerStopped;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    struct FakeProvider(AuthOutcome);

    impl AuthProvider for FakeProvider {
        fn authenticate(&mut self, mut secret: SecretBuffer) -> AuthOutcome {
            assert_eq!(secret.as_bytes(), b"test-secret");
            secret.clear();
            self.0
        }
    }

    #[test]
    fn worker_moves_secret_and_correlates_response() {
        let expected = AuthOutcome::Denied {
            stage: AuthStage::Authenticate,
            code: ErrorCode::AUTH_ERR,
        };
        let mut worker = AuthWorker::spawn(FakeProvider(expected)).unwrap();
        let mut secret = SecretBuffer::new(32).unwrap();
        secret.push_str("test-secret").unwrap();
        let attempt = worker.submit(secret).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(response) = worker.try_response().unwrap() {
                assert_eq!(response.attempt, attempt);
                assert_eq!(response.outcome, expected);
                break;
            }
            assert!(Instant::now() < deadline, "authentication worker timed out");
            thread::yield_now();
        }
    }

    #[test]
    fn pam_error_classification_is_fail_closed() {
        assert!(matches!(
            classify_error(AuthStage::Authenticate, ErrorCode::AUTH_ERR),
            AuthOutcome::Denied { .. }
        ));
        assert!(matches!(
            classify_error(AuthStage::Authenticate, ErrorCode::SYSTEM_ERR),
            AuthOutcome::Error { .. }
        ));
    }

    struct BlockingProvider {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    }

    impl AuthProvider for BlockingProvider {
        fn authenticate(&mut self, _secret: SecretBuffer) -> AuthOutcome {
            self.entered.wait();
            self.release.wait();
            AuthOutcome::Success
        }
    }

    #[test]
    fn worker_accepts_only_one_in_flight_attempt() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let mut worker = AuthWorker::spawn(BlockingProvider {
            entered: entered.clone(),
            release: release.clone(),
        })
        .unwrap();

        worker.submit(SecretBuffer::new(8).unwrap()).unwrap();
        entered.wait();
        let rejected = worker.submit(SecretBuffer::new(8).unwrap());
        assert!(matches!(rejected, Err(SubmitError::Busy(_))));
        release.wait();
    }

    struct PanickingProvider;

    impl AuthProvider for PanickingProvider {
        fn authenticate(&mut self, _secret: SecretBuffer) -> AuthOutcome {
            panic!("simulated PAM worker failure");
        }
    }

    #[test]
    fn worker_panic_disconnects_without_returning_success() {
        let mut worker = AuthWorker::spawn(PanickingProvider).unwrap();
        worker.submit(SecretBuffer::new(8).unwrap()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match worker.try_response() {
                Err(WorkerStopped) => break,
                Ok(None) => {
                    assert!(Instant::now() < deadline, "worker did not disconnect");
                    thread::yield_now();
                }
                Ok(Some(response)) => {
                    panic!("panicking provider returned a response: {response:?}")
                }
            }
        }
    }
}
