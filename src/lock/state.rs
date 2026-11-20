//! Fail-closed lifecycle for a lock attempt.

use std::error::Error;
use std::fmt;

/// Security-relevant state of one helium-locker invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockState {
    /// No backend has been asked to lock yet.
    Starting,
    /// The backend has been asked to lock, but has not confirmed coverage.
    LockRequested,
    /// The session is covered and input is controlled by the backend.
    Locked,
    /// PAM is processing an authentication attempt.
    Authenticating,
    /// PAM authentication and account validation both succeeded.
    UnlockAuthorized,
    /// The backend has been explicitly told to unlock.
    Unlocking,
    /// The backend confirmed completion of the unlock operation.
    Finished,
    /// Lock acquisition failed before the session was known to be locked.
    FailedToLock,
}

/// Events accepted by the lifecycle state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockEvent {
    RequestLock,
    LockConfirmed,
    LockRefused,
    BeginAuthentication,
    AuthenticationFailed,
    AuthenticationSucceeded,
    BeginUnlock,
    UnlockCompleted,
}

/// An event was attempted from a state in which it is not safe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidTransition {
    pub state: LockState,
    pub event: LockEvent,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "event {:?} is invalid in state {:?}",
            self.event, self.state
        )
    }
}

impl Error for InvalidTransition {}

/// Sole authority for moving a backend into its unlock path.
#[derive(Debug)]
pub struct LockMachine {
    state: LockState,
}

impl Default for LockMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl LockMachine {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: LockState::Starting,
        }
    }

    #[must_use]
    pub const fn state(&self) -> LockState {
        self.state
    }

    /// Whether the backend has confirmed that the session is locked and has
    /// not yet completed an authorized unlock.
    #[must_use]
    pub const fn is_locked(&self) -> bool {
        matches!(
            self.state,
            LockState::Locked
                | LockState::Authenticating
                | LockState::UnlockAuthorized
                | LockState::Unlocking
        )
    }

    /// Apply one lifecycle event. Invalid transitions leave the state intact.
    pub fn apply(&mut self, event: LockEvent) -> Result<LockState, InvalidTransition> {
        let next = match (self.state, event) {
            (LockState::Starting, LockEvent::RequestLock) => LockState::LockRequested,
            (LockState::LockRequested, LockEvent::LockConfirmed) => LockState::Locked,
            (LockState::LockRequested, LockEvent::LockRefused) => LockState::FailedToLock,
            (LockState::Locked, LockEvent::BeginAuthentication) => LockState::Authenticating,
            (LockState::Authenticating, LockEvent::AuthenticationFailed) => LockState::Locked,
            (LockState::Authenticating, LockEvent::AuthenticationSucceeded) => {
                LockState::UnlockAuthorized
            }
            (LockState::UnlockAuthorized, LockEvent::BeginUnlock) => LockState::Unlocking,
            (LockState::Unlocking, LockEvent::UnlockCompleted) => LockState::Finished,
            _ => {
                return Err(InvalidTransition {
                    state: self.state,
                    event,
                });
            }
        };

        self.state = next;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_flow_requires_every_security_gate() {
        let mut machine = LockMachine::new();

        assert_eq!(
            machine.apply(LockEvent::RequestLock),
            Ok(LockState::LockRequested)
        );
        assert!(!machine.is_locked());
        assert_eq!(
            machine.apply(LockEvent::LockConfirmed),
            Ok(LockState::Locked)
        );
        assert!(machine.is_locked());
        assert_eq!(
            machine.apply(LockEvent::BeginAuthentication),
            Ok(LockState::Authenticating)
        );
        assert_eq!(
            machine.apply(LockEvent::AuthenticationSucceeded),
            Ok(LockState::UnlockAuthorized)
        );
        assert_eq!(
            machine.apply(LockEvent::BeginUnlock),
            Ok(LockState::Unlocking)
        );
        assert!(machine.is_locked());
        assert_eq!(
            machine.apply(LockEvent::UnlockCompleted),
            Ok(LockState::Finished)
        );
        assert!(!machine.is_locked());
    }

    #[test]
    fn failed_authentication_returns_to_locked_and_can_retry() {
        let mut machine = locked_machine();

        machine.apply(LockEvent::BeginAuthentication).unwrap();
        machine.apply(LockEvent::AuthenticationFailed).unwrap();
        assert_eq!(machine.state(), LockState::Locked);
        assert!(machine.is_locked());

        machine.apply(LockEvent::BeginAuthentication).unwrap();
        assert_eq!(machine.state(), LockState::Authenticating);
    }

    #[test]
    fn lock_refusal_is_terminal_and_never_claims_coverage() {
        let mut machine = LockMachine::new();
        machine.apply(LockEvent::RequestLock).unwrap();
        machine.apply(LockEvent::LockRefused).unwrap();

        assert_eq!(machine.state(), LockState::FailedToLock);
        assert!(!machine.is_locked());
        assert!(machine.apply(LockEvent::BeginAuthentication).is_err());
    }

    #[test]
    fn unlock_cannot_begin_from_any_unauthorized_state() {
        let states = [
            LockState::Starting,
            LockState::LockRequested,
            LockState::Locked,
            LockState::Authenticating,
            LockState::Unlocking,
            LockState::Finished,
            LockState::FailedToLock,
        ];

        for state in states {
            let mut machine = LockMachine { state };
            let result = machine.apply(LockEvent::BeginUnlock);
            assert_eq!(
                result,
                Err(InvalidTransition {
                    state,
                    event: LockEvent::BeginUnlock
                })
            );
            assert_eq!(machine.state(), state);
        }
    }

    #[test]
    fn invalid_transition_does_not_mutate_state() {
        let mut machine = locked_machine();
        let error = machine.apply(LockEvent::UnlockCompleted).unwrap_err();

        assert_eq!(error.state, LockState::Locked);
        assert_eq!(machine.state(), LockState::Locked);
    }

    #[test]
    fn complete_transition_table_has_no_implicit_unlock_path() {
        let states = [
            LockState::Starting,
            LockState::LockRequested,
            LockState::Locked,
            LockState::Authenticating,
            LockState::UnlockAuthorized,
            LockState::Unlocking,
            LockState::Finished,
            LockState::FailedToLock,
        ];
        let events = [
            LockEvent::RequestLock,
            LockEvent::LockConfirmed,
            LockEvent::LockRefused,
            LockEvent::BeginAuthentication,
            LockEvent::AuthenticationFailed,
            LockEvent::AuthenticationSucceeded,
            LockEvent::BeginUnlock,
            LockEvent::UnlockCompleted,
        ];

        for state in states {
            for event in events {
                let expected = match (state, event) {
                    (LockState::Starting, LockEvent::RequestLock) => Some(LockState::LockRequested),
                    (LockState::LockRequested, LockEvent::LockConfirmed) => Some(LockState::Locked),
                    (LockState::LockRequested, LockEvent::LockRefused) => {
                        Some(LockState::FailedToLock)
                    }
                    (LockState::Locked, LockEvent::BeginAuthentication) => {
                        Some(LockState::Authenticating)
                    }
                    (LockState::Authenticating, LockEvent::AuthenticationFailed) => {
                        Some(LockState::Locked)
                    }
                    (LockState::Authenticating, LockEvent::AuthenticationSucceeded) => {
                        Some(LockState::UnlockAuthorized)
                    }
                    (LockState::UnlockAuthorized, LockEvent::BeginUnlock) => {
                        Some(LockState::Unlocking)
                    }
                    (LockState::Unlocking, LockEvent::UnlockCompleted) => Some(LockState::Finished),
                    _ => None,
                };

                let mut machine = LockMachine { state };
                match expected {
                    Some(next) => assert_eq!(machine.apply(event), Ok(next)),
                    None => {
                        assert_eq!(
                            machine.apply(event),
                            Err(InvalidTransition { state, event })
                        );
                        assert_eq!(machine.state(), state);
                    }
                }
            }
        }
    }

    fn locked_machine() -> LockMachine {
        let mut machine = LockMachine::new();
        machine.apply(LockEvent::RequestLock).unwrap();
        machine.apply(LockEvent::LockConfirmed).unwrap();
        machine
    }
}
