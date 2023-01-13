use crate::feed::{Feed, FeedError};
use crate::state::{BoxedState, DeliveryStatus, StateTypes, Transition};
use serde::{Deserialize, Serialize};
use std::thread::sleep;
use std::time::Duration;
use thiserror::Error;
use std::sync::mpsc;

#[cfg(feature = "tracing")]
use tracing::Span;

/// State machine that takes an initial states and attempts to advance it until the terminal state.
pub struct StateMachine<Types: StateTypes> {
    /// id for this state machine.
    id: StateMachineId,

    /// The current state.
    state: BoxedState<Types>,

    /// [Feed] combining delayed messages and the external feed.
    feed: Feed<Types::In>,

    /// Channel to report outgoing messages.
    messages_tx: mpsc::Sender<Messages<Types::Out>>,

    #[cfg(feature = "tracing")]
    span: Span,
}

impl<Types> StateMachine<Types>
where
    Types: 'static + StateTypes,
{
    /// Create a new [StateMachine] and a corresponding [StateMachineHandle].
    pub fn new(
        id: StateMachineId,
        initial_state: BoxedState<Types>,
        messages_tx: mpsc::Sender<Messages<Types::Out>>,
        #[cfg(feature = "tracing")] span: Span,
    ) -> (Self, StateMachineHandle<Types>) {
        log::trace!(
            "[{id:?}] State machine has been initialized at <{}>",
            initial_state.desc()
        );

        let (tx, rx) = mpsc::channel();
        let handle = StateMachineHandle { tx };

        (
            Self::new_with_handle(
                id,
                initial_state,
                messages_tx,
                rx,
                #[cfg(feature = "tracing")]
                span,
            ),
            handle,
        )
    }

    /// Create a new [StateMachine] and with a receiver channel corresponding to a given [StateMachineHandle].
    /// This method can be useful when the receiver channel must be known before a [StateMachine] can be constructed,
    /// e.g., when state machines are executed one after another, such that the result of the former [StateMachine]
    /// is used for construction of an initial state for the latter [StateMachine].
    pub fn new_with_handle(
        id: StateMachineId,
        initial_state: BoxedState<Types>,
        messages_tx: mpsc::Sender<Messages<Types::Out>>,
        handle_rx: mpsc::Receiver<Types::In>,
        #[cfg(feature = "tracing")] span: Span,
    ) -> Self {
        Self {
            id,
            state: initial_state,
            feed: Feed::new(handle_rx),
            messages_tx,

            #[cfg(feature = "tracing")]
            span,
        }
    }

    /// Attempt to execute this [StateMachine].
    pub fn execute(mut self) -> StateMachineResult<Types> {
        let value = match self.run() {
            Ok(()) => Ok(self.state),
            Err(StateMachineDriverError::StateError(error)) => Err(StateMachineError::State {
                error,
                state: self.state,
                feed: self.feed,
            }),
            Err(StateMachineDriverError::IncomingCommunication(err)) => {
                Err(StateMachineError::IncomingCommunication(err))
            }
            Err(StateMachineDriverError::OutgoingCommunication(err)) => {
                Err(StateMachineError::OutgoingCommunication(err))
            }
        };

        StateMachineResult {
            value,
            #[cfg(feature = "tracing")]
            span: self.span,
        }
    }

    #[cfg_attr(feature = "tracing", tracing::instrument[parent = &self.span, skip_all])]
    /// Executes this [StateMachine] regardless of time. Publicly inaccessible,
    fn run(&mut self) -> Result<(), StateMachineDriverError<Types>> {
        let Self {
            id,
            ref mut state,
            ref mut feed,
            ref mut messages_tx,

            #[cfg(feature = "tracing")]
            span,
        } = self;

        log::debug!("[{id:?}] State machine is running");

        let mut is_state_initialized = false;

        loop {
            // Try to Initialize the state.
            if !is_state_initialized {
                log::trace!("[{:?}] Initializing", id);

                let messages = state.initialize();

                messages_tx
                    .send(Messages {
                        from: id.clone(),
                        value: messages,
                        #[cfg(feature = "tracing")]
                        span: span.clone(),
                    })
                    .map_err(|err| StateMachineDriverError::OutgoingCommunication(err.0.value))?;

                is_state_initialized = true;
            }

            // Attempt to update.
            log::trace!("[{id:?}] State update attempt");
            state.update().map_err(|err| {
                log::debug!("[{id:?}] State update attempt failed with: {err:?}");
                StateMachineDriverError::StateError(err)
            })?;

            // Attempt to advance.
            log::trace!("[{id:?}] State advance attempt");
            let advanced = state.advance().map_err(|err| {
                log::debug!("[{id:?}] State advance attempt failed with: {err:?}");
                StateMachineDriverError::StateError(err)
            })?;

            match advanced {
                Transition::Same => {
                    log::trace!("[{id:?}] State requires more input");

                    // No advancement. Try to deliver a message.
                    match feed.next() {
                        Ok(message) => {
                            match state.deliver(message) {
                                DeliveryStatus::Delivered => {
                                    log::trace!("[{id:?}] Message has been delivered");
                                }
                                DeliveryStatus::Unexpected(message) => {
                                    log::trace!("[{id:?}] Unexpected message. Storing for future attempts");
                                    feed.delay(message);
                                }
                                DeliveryStatus::Rejected(_message) => {
                                    log::trace!("[{id:?}] Unexpected message. It will be dropped");
                                }
                                DeliveryStatus::Error(err) => {
                                    Err(StateMachineDriverError::StateError(err))?;
                                }
                            }
                        },
                        Err(FeedError::NoMessage) => {
                            sleep(Duration::from_millis(10));
                            continue;
                        },
                        Err(err) => Err(StateMachineDriverError::IncomingCommunication(err))?
                    };
                }
                Transition::Next(next) => {
                    log::debug!("[{id:?}] State has been advanced to <{}>", next.desc());

                    // Update the current state.
                    *state = next;
                    is_state_initialized = false;

                    // Refresh the feed.
                    feed.refresh();
                }
                Transition::Terminal => {
                    log::debug!("[{id:?}] State is terminal. Completing...");
                    break;
                }
            }
        }

        log::debug!("[{id:?}] State machine has completed");
        Ok(())
    }
}

#[derive(Debug)]
/// [StateMachineHandle] is a synchronous interface to communicate message to a corresponding [StateMachine].
pub struct StateMachineHandle<Types: StateTypes> {
    tx: mpsc::Sender<Types::In>,
}

impl<Types: StateTypes> From<mpsc::Sender<Types::In>> for StateMachineHandle<Types> {
    fn from(tx: mpsc::Sender<Types::In>) -> Self {
        Self { tx }
    }
}

impl<Types: StateTypes> StateMachineHandle<Types> {
    /// Attempts a message delivery, if unsuccessful, returns message back as Err(message).
    pub fn deliver(&self, message: Types::In) -> Result<(), Types::In> {
        self.tx.send(message).map_err(|err| err.0)
    }
}

/// Messages emitted from a [StateMachine] are enriched with some contextual information.
pub struct Messages<T> {
    /// Id of the [StateMachine] emitting the messages.
    pub from: StateMachineId,
    /// Actual messages.
    pub value: Vec<T>,

    #[cfg(feature = "tracing")]
    pub span: Span,
}

/// Enriched result of the [StateMachine] execution.
pub struct StateMachineResult<T: StateTypes> {
    pub value: Result<BoxedState<T>, StateMachineError<T>>,

    #[cfg(feature = "tracing")]
    pub span: Span,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct StateMachineId(String);

impl From<&str> for StateMachineId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for StateMachineId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl StateMachineId {
    pub fn id(&self) -> &str {
        &self.0
    }
}

/// Possible reasons for erroneous run of the state machine.
/// This enum provides a rich context in which the state machine run.
#[derive(Error, Debug)]
pub enum StateMachineError<Types: StateTypes> {
    #[error("{0:?}")]
    IncomingCommunication(FeedError),

    #[error("Impossible to send out outgoing messages")]
    OutgoingCommunication(Vec<Types::Out>),

    #[error("Internal state error: {error:?}")]
    State {
        error: Types::Err,
        state: BoxedState<Types>,
        feed: Feed<Types::In>,
    },
}

enum StateMachineDriverError<Types: StateTypes> {
    IncomingCommunication(FeedError),
    OutgoingCommunication(Vec<Types::Out>),
    StateError(Types::Err),
}

#[cfg(test)]
mod test {
    use crate::state::{DeliveryStatus, State, StateTypes, Transition};
    use crate::state_machine::{StateMachine, StateMachineError, StateMachineResult};
    use pretty_env_logger;
    use std::collections::VecDeque;
    use std::thread::{spawn, sleep};
    use std::time::Duration;
    use std::sync::mpsc;

    #[cfg(feature = "tracing")]
    use tracing::info_span;

    pub fn setup_logging_or_tracing(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        if cfg!(feature = "tracing") {
            tracing_subscriber::fmt::try_init()
        } else {
            pretty_env_logger::try_init().map_err(|e| e.into())
        }
    }

    /// This module tests an order state machine.
    /// Item's lifecycle
    /// OnDisplay -(PlaceOrder)-> Placed -(Verify)-> Verified -(FinalConfirmation)-> Shipped
    ///    ↑           ↳(Cancel)-> Canceled             ↳(Cancel)-> Canceled
    ///    └-------------------------┘---------------------------------┘

    struct Types;
    impl StateTypes for Types {
        type In = Message;
        type Out = ();
        type Err = String;
    }

    #[derive(Debug)]
    enum Message {
        PlaceOrder,
        Damage,
        Verify(Verify),
        FinalConfirmation,
        Cancel,
    }

    #[derive(Debug)]
    enum Verify {
        Address,
        PaymentDetails,
    }

    // `OnDisplay` state ===========
    #[derive(Debug)]
    struct OnDisplay {
        is_ordered: bool,
        is_broken: bool,
    }

    impl OnDisplay {
        fn new() -> Self {
            Self {
                is_ordered: false,
                is_broken: false,
            }
        }
    }

    impl State<Types> for OnDisplay {
        fn desc(&self) -> String {
            "Item is on display for selling".to_string()
        }

        fn deliver(
            &mut self,
            message: Message,
        ) -> DeliveryStatus<Message, <Types as StateTypes>::Err> {
            match message {
                Message::PlaceOrder => self.is_ordered = true,
                Message::Damage => self.is_broken = true,
                _ => return DeliveryStatus::Unexpected(message),
            }
            DeliveryStatus::Delivered
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            if self.is_broken {
                return Err("Item is damaged".to_string());
            }

            Ok(if self.is_ordered {
                Transition::Next(Box::new(Placed::new()))
            } else {
                Transition::Same
            })
        }
    }
    // ========================

    // `Placed` state ===========
    #[derive(Debug)]
    struct Placed {
        is_address_present: bool,
        is_payment_ok: bool,
        is_canceled: bool,
    }

    impl Placed {
        fn new() -> Self {
            Self {
                is_address_present: false,
                is_payment_ok: false,
                is_canceled: false,
            }
        }
    }

    impl State<Types> for Placed {
        fn desc(&self) -> String {
            "Order has been placed, awaiting verification.".to_string()
        }

        fn deliver(
            &mut self,
            message: Message,
        ) -> DeliveryStatus<Message, <Types as StateTypes>::Err> {
            match message {
                Message::Cancel => self.is_canceled = true,
                Message::Verify(Verify::Address) => self.is_address_present = true,
                Message::Verify(Verify::PaymentDetails) => self.is_payment_ok = true,
                _ => return DeliveryStatus::Unexpected(message),
            }

            DeliveryStatus::Delivered
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            if self.is_canceled {
                return Ok(Transition::Next(Box::new(Canceled)));
            }

            Ok(if self.is_payment_ok && self.is_address_present {
                Transition::Next(Box::new(Verified::new()))
            } else {
                Transition::Same
            })
        }
    }
    // ========================

    // `Verified` state ===========
    #[derive(Debug)]
    struct Verified {
        is_canceled: bool,
        is_confirmed: bool,
    }

    impl Verified {
        fn new() -> Self {
            Self {
                is_canceled: false,
                is_confirmed: false,
            }
        }
    }

    impl State<Types> for Verified {
        fn desc(&self) -> String {
            "Order has all required details, awaiting the final confirmation".to_string()
        }

        fn deliver(
            &mut self,
            message: Message,
        ) -> DeliveryStatus<Message, <Types as StateTypes>::Err> {
            match message {
                Message::Cancel => self.is_canceled = true,
                Message::FinalConfirmation => self.is_confirmed = true,
                _ => return DeliveryStatus::Unexpected(message),
            }

            DeliveryStatus::Delivered
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            if self.is_canceled {
                return Ok(Transition::Next(Box::new(Canceled)));
            }

            Ok(if self.is_confirmed {
                Transition::Next(Box::new(Shipped))
            } else {
                Transition::Same
            })
        }
    }
    // ========================

    // `Shipped` state ===========
    #[derive(Debug)]
    struct Shipped;

    impl State<Types> for Shipped {
        fn desc(&self) -> String {
            "Order has been shipped".to_string()
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            Ok(Transition::Terminal)
        }
    }
    // ========================

    // `Shipped` state ===========
    #[derive(Debug)]
    struct Canceled;

    impl State<Types> for Canceled {
        fn desc(&self) -> String {
            "Order has been canceled".to_string()
        }

        fn advance(&self) -> Result<Transition<Types>, <Types as StateTypes>::Err> {
            Ok(Transition::Next(Box::new(OnDisplay::new())))
        }
    }
    // ========================

    #[test]
    fn order_goes_to_shipping() {
        let _ = setup_logging_or_tracing();

        // initial state.
        let on_display = OnDisplay::new();

        // feed of un-ordered messages.
        let mut feed = VecDeque::from(vec![
            Message::Verify(Verify::Address),
            Message::PlaceOrder,
            Message::FinalConfirmation,
            Message::Verify(Verify::PaymentDetails),
        ]);
        let feeding_interval = Duration::from_millis(100);

        #[cfg(feature = "tracing")]
        let span = info_span!("test_span");

        let (messages_tx, _messages_rx) = mpsc::channel();
        let (state_machine, state_machine_handle) = StateMachine::new(
            "Order".into(),
            Box::new(on_display),
            messages_tx,
            // Add Span when feature 'tracing' is enabled
            #[cfg(feature = "tracing")]
            span,
        );

        let (tx, mut rx) = mpsc::channel();
        spawn(move || {
            let result = state_machine.execute();
            tx.send(result).unwrap_or_else(|_| panic!("Must send"));
        });


        let res: StateMachineResult<Types> = loop {
            sleep(feeding_interval);
            if let Ok(r) = rx.try_recv() {
                break r
            } else if feed.is_empty() {
                panic!("state machine didn't complete!")
            }

            if let Some(msg) = feed.pop_front() {
                let _ = state_machine_handle.deliver(msg);
            } 
        };

        let order = res
            .value
            .unwrap_or_else(|_| panic!("State machine did not complete in time"));
        assert!(order.is::<Shipped>());

        let _shipped = order
            .downcast::<Shipped>()
            .unwrap_or_else(|_| panic!("must work"));
    }

}
