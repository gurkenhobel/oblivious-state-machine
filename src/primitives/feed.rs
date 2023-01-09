use std::collections::VecDeque;
use std::fmt::Debug;
use thiserror::Error;
use std::sync::mpsc::{self, TryRecvError};

/// [Feed] combines polling from a queue of messages and a channel. Message can be delayed
/// and later placed in the queue.
#[derive(Debug)]
pub struct Feed<T> {
    /// Messages from [queue] will be delivered first.
    queue: VecDeque<T>,

    /// Channel to receive message to deliver..
    feed: mpsc::Receiver<T>,

    /// Any message drawn from [Feed] can be delayed and later placed in the [queue].
    delayed: Vec<T>,
}

impl<T> Feed<T> {
    pub(crate) fn new(feed: mpsc::Receiver<T>) -> Self {
        Self {
            queue: VecDeque::new(),
            feed,
            delayed: Vec::new(),
        }
    }

    /// Draw the next message either from [queue] or [feed].
    pub(crate) fn next(&mut self) -> Result<T, FeedError> {
        if !self.queue.is_empty() {
            return self
                .queue
                .pop_front()
                .ok_or_else(|| panic!("Popping a message from a non-empty queue must not fail"));
        }

        match self.feed.try_recv() {
            Ok(msg) => Ok(msg),
            Err(TryRecvError::Empty)  => Err(FeedError::NoMessage),
            Err(TryRecvError::Disconnected) => Err(FeedError::ChannelClosed)
        }
    }

    pub(crate) fn delay(&mut self, message: T) {
        self.delayed.push(message);
    }

    /// Place [delayed] messages in the [queue].
    pub(crate) fn refresh(&mut self) {
        self.delayed
            .drain(..)
            .rev()
            .for_each(|message| self.queue.push_front(message));
    }
}

#[derive(Error, Debug)]
pub enum FeedError {
    #[error("Channel has been closed prematurely; more messages are expected")]
    ChannelClosed,
    #[error("Channel has currently no message; more messages are expected")]
    NoMessage,
}
