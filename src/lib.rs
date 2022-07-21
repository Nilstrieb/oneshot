//! Oneshot spsc channel. The sender's send method is non-blocking, lock- and wait-free[1].
//! The receiver supports both lock- and wait-free `try_recv` as well as indefinite and time
//! limited thread blocking receive operations. The receiver also implements `Future` and
//! supports asynchronously awaiting the message.
//!
//! This is a oneshot channel implementation. Meaning each channel instance can only transport
//! a single message. This has a few nice outcomes. One thing is that the implementation can
//! be very efficient, utilizing the knowledge that there will only be one message. But more
//! importantly, it allows the API to be expressed in such a way that certain edge cases
//! that you don't want to care about when only sending a single message on a channel does not
//! exist. For example. The sender can't be copied or cloned and the send method takes ownership
//! and consumes the sender. So you are guaranteed, at the type level, that there can only be
//! one message sent.
//!
//! # Examples
//!
//! A very basic example to just show the API:
//!
//! ```rust
//! # #[cfg(not(feature = "sync"))]
//! # fn main() {}
//! # #[cfg(feature = "sync")]
//! # fn main() {
//! # use std::thread;
//! let (sender, receiver) = oneshot::channel();
//! thread::spawn(move || {
//!     sender.send("Hello from worker thread!");
//! });
//!
//! let message = receiver.recv().expect("Worker thread does not want to talk :(");
//! println!("A message from a different thread: {}", message);
//! # }
//! ```
//!
//! A slightly larger example showing communicating back work of *different types* during a
//! long computation. The final result here could have been communicated back via the thread's
//! `JoinHandle`. But those can't be waited on with a timeout. This is a quite artificial example,
//! that mostly shows the API.
//!
//! ```rust
//! # #[cfg(not(feature = "sync"))]
//! # fn main() {}
//! # #[cfg(feature = "sync")]
//! # fn main() {
//! # use core::time::Duration;
//! # use std::thread;
//! # fn expensive_initialization() -> Data { Data }
//! # struct Data;
//! # impl Data {
//! #     fn summary(&self) -> &'static str { "" }
//! #     fn expensive_computation(self) -> Vec<u8> { Vec::new() }
//! # }
//! let (sender1, receiver1) = oneshot::channel();
//! let (sender2, receiver2) = oneshot::channel();
//!
//! let thread = thread::spawn(move || {
//!     let data_processor = expensive_initialization();
//!     sender1.send(data_processor.summary()).expect("Main thread not waiting");
//!     sender2.send(data_processor.expensive_computation()).expect("Main thread not waiting");
//! });
//!
//! let summary = receiver1.recv().expect("Worker thread died");
//! println!("Initialized data. Will crunch these numbers: {}", summary);
//!
//! let result = loop {
//!     match receiver2.recv_timeout(Duration::from_secs(1)) {
//!         Ok(result) => break result,
//!         Err(oneshot::RecvTimeoutError::Timeout) => println!("Still working..."),
//!         Err(oneshot::RecvTimeoutError::Disconnected) => panic!("Worker thread died"),
//!     }
//! };
//! println!("Done computing. Results: {:?}", result);
//! thread.join().expect("Worker thread panicked");
//! # }
//! ```
//!
//! # Sync vs async
//!
//! The main motivation for writing this library was that there were no (known to me) channel
//! implementations allowing you to seamlessly send messages between a normal thread and an async
//! task, or the other way around. If message passing is the way you are communicating, of course
//! that should work smoothly between the sync and async parts of the program!
//!
//! This library achieves that by having an almost[1] wait-free send operation that can safely
//! be used in both sync threads and async tasks. The receiver has both thread blocking
//! receive methods for synchronous usage, and implements `Future` for asynchronous usage.
//!
//! The receiving endpoint of this channel implements Rust's `Future` trait and can be waited on
//! in an asynchronous task. This implementation is completely executor/runtime agnostic. It should
//! be possible to use this library with any executor.
//!
//! # Footnotes
//!
//! [1]: See documentation on [Sender::send] for situations where it might not be fully wait-free.

// # Implementation description
//
// When a channel is created via the channel function, it allocates space on the heap to fit:
// * A one byte atomic integer that represents the current channel state,
// * Uninitialized memory to fit the message,
// * Uninitialized memory to fit the waker that can wake the receiving task or thread up.
//
// The size of the waker depends on which features are activated, it ranges from 0 to 24 bytes[1].
// So with all features enabled (the default) each channel allocates 25 bytes plus the size of the
// message, plus any padding needed to get correct memory alignment.
//
// The Sender and Receiver only holds a raw pointer to this heap channel object. The last endpoint
// to be consumed or dropped is responsible for freeing the heap memory. The first endpoint to
// go away signal via the state that it is gone. And the second one see this and frees the memory.
//
// Sending on the sender copies the message to the (so far uninitialized) memory region on the
// heap and swaps the state from whatever it was to MESSAGE.
// if the state before the swap was DISCONNECTED the SendError is returned and nothing else is done.
// The SendError now owns the heap channel memory and is responsible for dropping the message
// and freeing the memory.
// If the state was RECEIVING the sender reads the waker object from the channel heap memory and
// call the unpark method, which will wake up the receiver.
//
// Receiving on the channel first checks the state. If it is MESSAGE the message object is read
// from the heap back into the stack, the heap memory is freed and the message returned. If the
// state is DISCONNECTED the heap memory is freed and an error is returned. And if the state is
// EMPTY and the receive operation is a blocking one it creates a waker object and writes it to
// the channel on the heap and does an atomic compare_and_swap on the state from EMPTY to RECEIVING.
// If the swap went fine, it either parks the thread or returns Poll::Pending, depending on if
// the receive is a blocking or an async one. It now just waits for the sender to wake it up.
//
//
// ## Footnotes
//
// [1]: Mind that the waker only takes zero bytes when all features are disabled, making it
//      impossible to *wait* for the message. `try_recv` the only available method in this scenario.

#![deny(rust_2018_idioms)]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(loom))]
extern crate alloc;

use core::{
    marker::PhantomData,
    mem::{self, MaybeUninit},
    ptr::{self, NonNull},
};

#[cfg(not(loom))]
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU8, Ordering::SeqCst},
};
#[cfg(loom)]
use loom::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU8, Ordering::SeqCst},
};

#[cfg(feature = "async")]
use core::{
    pin::Pin,
    task::{self, Poll},
};
#[cfg(feature = "std")]
use std::time::{Duration, Instant};

#[cfg(feature = "std")]
mod thread {
    #[cfg(not(loom))]
    pub use std::thread::{current, park, park_timeout, Thread};

    #[cfg(loom)]
    pub use loom::thread::{current, park, Thread};

    // loom does not support parking with a timeout. So we just
    // yield. This means that the "park" will "spuriously" wake up
    // way too early. But the code should properly handle this.
    // One thing to note is that very short timeouts are needed
    // when using loom, since otherwise the looping will cause
    // an overflow in loom.
    #[cfg(loom)]
    pub fn park_timeout(_timeout: std::time::Duration) {
        loom::thread::yield_now()
    }
}

#[cfg(loom)]
mod loombox;
#[cfg(not(loom))]
use alloc::boxed::Box;
#[cfg(loom)]
use loombox::Box;

mod errors;
pub use errors::{RecvError, RecvTimeoutError, SendError, TryRecvError};

/// Creates a new oneshot channel and returns the two endpoints, [`Sender`] and [`Receiver`].
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    // Allocate the channel on the heap and get the pointer.
    // The last endpoint of the channel to be alive is responsible for freeing the channel
    // and dropping any object that might have been written to it.

    let channel_ptr = Box::into_raw(Box::new(Channel::new()));

    // SAFETY: `channel_ptr` came from a Box and thus is not null
    let channel_ptr = unsafe { NonNull::new_unchecked(channel_ptr) };

    (
        Sender {
            channel_ptr,
            _invariant: PhantomData,
            _dropck: PhantomData,
        },
        Receiver {
            channel_ptr,
            _dropck: PhantomData,
        },
    )
}

#[derive(Debug)]
pub struct Sender<T> {
    channel_ptr: NonNull<Channel<T>>,
    // In reality we want contravariance, however we can't obtain that.
    //
    // Consider the following scenario:
    // ```
    // let (mut tx, rx) = channel::<&'short u8>();
    // let (tx2, rx2) = channel::<&'long u8>();
    //
    // tx = tx2;
    //
    // // Pretend short_ref is some &'short u8
    // tx.send(short_ref).unwrap();
    // let long_ref = rx2.recv().unwrap();
    // ```
    //
    // If this type were covariant then we could safely extend lifetimes, which is not okay.
    // Hence, we enforce invariance.
    _invariant: PhantomData<fn(T) -> T>,
    // See SendError for details
    _dropck: PhantomData<T>,
}

#[derive(Debug)]
pub struct Receiver<T> {
    // Covariance is the right choice here. Consider the example presented in Sender, and you'll
    // see that if we replaced `rx` instead then we would get the expected behavior
    channel_ptr: NonNull<Channel<T>>,
    // See SendError for details
    _dropck: PhantomData<T>,
}

unsafe impl<T: Send> Send for Sender<T> {}
unsafe impl<T: Send> Send for Receiver<T> {}
impl<T> Unpin for Receiver<T> {}

impl<T> Sender<T> {
    /// Sends `message` over the channel to the corresponding [`Receiver`].
    ///
    /// Returns an error if the receiver has already been dropped. The message can
    /// be extracted from the error.
    ///
    /// This method is completely lock-free and wait-free when sending on a channel that the
    /// receiver is currently not receiving on. If the receiver is receiving during the send
    /// operation this method includes waking up the thread/task. Unparking a thread currently
    /// involves a mutex in Rust's standard library. How lock-free waking up an async task is
    /// depends on your executor. If this method returns a `SendError`, please mind that dropping
    /// the error involves running any drop implementation on the message type, which might or
    /// might not be lock-free.
    pub fn send(self, message: T) -> Result<(), SendError<T>> {
        let channel_ptr = self.channel_ptr;

        // Don't run our Drop implementation if send was called, any cleanup now happens here
        mem::forget(self);

        let channel = unsafe { channel_ptr.as_ref() };

        // Write the message into the channel on the heap.
        unsafe { channel.write_message(message) };

        // Set the state to signal there is a message on the channel.
        match channel.state.swap(MESSAGE, SeqCst) {
            // The receiver is alive and has not started waiting. Send done.
            EMPTY => Ok(()),
            // The receiver is waiting. Wake it up so it can return the message.
            RECEIVING => {
                unsafe { channel.take_waker() }.unpark();
                Ok(())
            }
            // The receiver was already dropped. The error is responsible for freeing the channel.
            DISCONNECTED => Err(unsafe { SendError::new(channel_ptr) }),
            _ => unreachable!(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // SAFETY: The reference won't be used after the channel is freed in this method
        let channel = unsafe { self.channel_ptr.as_ref() };

        // Set the channel state to disconnected and read what state the receiver was in
        match channel.state.swap(DISCONNECTED, SeqCst) {
            // The receiver has not started waiting, nor is it dropped.
            EMPTY => (),
            // The receiver is waiting. Wake it up so it can detect that the channel disconnected.
            RECEIVING => unsafe { channel.take_waker() }.unpark(),
            // The receiver was already dropped. We are responsible for freeing the channel.
            DISCONNECTED => {
                unsafe { dealloc(self.channel_ptr) };
            }
            _ => unreachable!(),
        }
    }
}

impl<T> Receiver<T> {
    /// Checks if there is a message in the channel without blocking. Returns:
    ///  * `Ok(message)` if there was a message in the channel.
    ///  * `Err(Empty)` if the [`Sender`] is alive, but has not yet sent a message.
    ///  * `Err(Disconnected)` if the [`Sender`] was dropped before sending anything or if the
    ///    message has already been extracted by a previous receive call.
    ///
    /// If a message is returned, the channel is disconnected and any subsequent receive operation
    /// using this receiver will return an error.
    ///
    /// This method is completely lock-free and wait-free. The only thing it does is an atomic
    /// integer load of the channel state. And if there is a message in the channel it additionally
    /// performs one atomic integer store and copies the message from the heap to the stack for
    /// returning it.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        // SAFETY: The channel will not be freed while this method is still running.
        let channel = unsafe { self.channel_ptr.as_ref() };

        match channel.state.load(SeqCst) {
            // The sender is alive but has not sent anything yet.
            EMPTY => Err(TryRecvError::Empty),
            // The sender sent the message. We take the message and mark the channel disconnected.
            MESSAGE => {
                channel.state.store(DISCONNECTED, SeqCst);
                Ok(unsafe { channel.take_message() })
            }
            // The sender was dropped before sending anything, or we already received the message.
            DISCONNECTED => Err(TryRecvError::Disconnected),
            // The receiver must have already been `Future::poll`ed. No message available.
            #[cfg(feature = "async")]
            RECEIVING => Err(TryRecvError::Empty),
            _ => unreachable!(),
        }
    }

    /// Attempts to wait for a message from the [`Sender`], returning an error if the channel is
    /// disconnected.
    ///
    /// This method will always block the current thread if there is no data available and it is
    /// still possible for the message to be sent. Once the message is sent to the corresponding
    /// [`Sender`], then this receiver will wake up and return that message.
    ///
    /// If the corresponding [`Sender`] has disconnected (been dropped), or it disconnects while
    /// this call is blocking, this call will wake up and return `Err` to indicate that the message
    /// can never be received on this channel.
    ///
    /// If a sent message has already been extracted from this channel this method will return an
    /// error.
    ///
    /// # Panics
    ///
    /// Panics if called after this receiver has been polled asynchronously.
    #[cfg(feature = "std")]
    pub fn recv(self) -> Result<T, RecvError> {
        let channel_ptr = self.channel_ptr;

        // Don't run our Drop implementation if we are receiving consuming ourselves.
        mem::forget(self);

        let channel = unsafe { channel_ptr.as_ref() };

        match channel.state.load(SeqCst) {
            // The sender is alive but has not sent anything yet. We prepare to park.
            EMPTY => {
                // Conditionally add a delay here to help the tests trigger the edge cases where
                // the sender manages to be dropped or send something before we are able to store
                // our waker object in the channel.
                #[cfg(oneshot_test_delay)]
                std::thread::sleep(std::time::Duration::from_millis(10));

                // Write our waker instance to the channel.
                unsafe { channel.write_waker(ReceiverWaker::current_thread()) };

                match channel
                    .state
                    .compare_exchange(EMPTY, RECEIVING, SeqCst, SeqCst)
                {
                    // We stored our waker, now we park until the sender has changed the state
                    Ok(EMPTY) => loop {
                        thread::park();
                        match channel.state.load(SeqCst) {
                            // The sender sent the message while we were parked.
                            MESSAGE => {
                                let message = unsafe { channel.take_message() };
                                unsafe { dealloc(channel_ptr) };
                                break Ok(message);
                            }
                            // The sender was dropped while we were parked.
                            DISCONNECTED => {
                                unsafe { dealloc(channel_ptr) };
                                break Err(RecvError);
                            }
                            // State did not change, spurious wakeup, park again.
                            RECEIVING => (),
                            _ => unreachable!(),
                        }
                    },
                    // The sender sent the message while we prepared to park.
                    Err(MESSAGE) => {
                        unsafe { channel.drop_waker() };
                        let message = unsafe { channel.take_message() };
                        unsafe { dealloc(channel_ptr) };
                        Ok(message)
                    }
                    // The sender was dropped before sending anything while we prepared to park.
                    Err(DISCONNECTED) => {
                        unsafe { channel.drop_waker() };
                        unsafe { dealloc(channel_ptr) };
                        Err(RecvError)
                    }
                    _ => unreachable!(),
                }
            }
            // The sender already sent the message.
            MESSAGE => {
                let message = unsafe { channel.take_message() };
                unsafe { dealloc(channel_ptr) };
                Ok(message)
            }
            // The sender was dropped before sending anything, or we already received the message.
            DISCONNECTED => {
                unsafe { dealloc(channel_ptr) };
                Err(RecvError)
            }
            // The receiver must have been `Future::poll`ed prior to this call.
            #[cfg(feature = "async")]
            RECEIVING => panic!("{}", RECEIVER_USED_SYNC_AND_ASYNC_ERROR),
            _ => unreachable!(),
        }
    }

    /// Attempts to wait for a message from the [`Sender`], returning an error if the channel is
    /// disconnected. This is a non consuming version of [`Receiver::recv`], but with a bit
    /// worse performance. Prefer `[`Receiver::recv`]` if your code allows consuming the receiver.
    ///
    /// If a message is returned, the channel is disconnected and any subsequent receive operation
    /// using this receiver will return an error.
    ///
    /// # Panics
    ///
    /// Panics if called after this receiver has been polled asynchronously.
    #[cfg(feature = "std")]
    pub fn recv_ref(&self) -> Result<T, RecvError> {
        let channel_ptr = self.channel_ptr;
        let channel = unsafe { channel_ptr.as_ref() };

        match channel.state.load(SeqCst) {
            // The sender is alive but has not sent anything yet. We prepare to park.
            EMPTY => {
                // Conditionally add a delay here to help the tests trigger the edge cases where
                // the sender manages to be dropped or send something before we are able to store
                // our waker object in the channel.
                #[cfg(oneshot_test_delay)]
                std::thread::sleep(std::time::Duration::from_millis(10));

                // Write our waker instance to the channel.
                unsafe { channel.write_waker(ReceiverWaker::current_thread()) };

                match channel
                    .state
                    .compare_exchange(EMPTY, RECEIVING, SeqCst, SeqCst)
                {
                    // We stored our waker, now we park until the sender has changed the state
                    Ok(EMPTY) => loop {
                        thread::park();
                        match channel.state.load(SeqCst) {
                            // The sender sent the message while we were parked.
                            // We take the message and mark the channel disconnected.
                            MESSAGE => {
                                channel.state.store(DISCONNECTED, SeqCst);
                                break Ok(unsafe { channel.take_message() });
                            }
                            // The sender was dropped while we were parked.
                            DISCONNECTED => break Err(RecvError),
                            // State did not change, spurious wakeup, park again.
                            RECEIVING => (),
                            _ => unreachable!(),
                        }
                    },
                    // The sender sent the message while we prepared to park.
                    Err(MESSAGE) => {
                        channel.state.store(DISCONNECTED, SeqCst);
                        unsafe { channel.drop_waker() };
                        Ok(unsafe { channel.take_message() })
                    }
                    // The sender was dropped before sending anything while we prepared to park.
                    Err(DISCONNECTED) => {
                        unsafe { channel.drop_waker() };
                        Err(RecvError)
                    }
                    _ => unreachable!(),
                }
            }
            // The sender sent the message. We take the message and mark the channel disconnected.
            MESSAGE => {
                channel.state.store(DISCONNECTED, SeqCst);
                Ok(unsafe { channel.take_message() })
            }
            // The sender was dropped before sending anything, or we already received the message.
            DISCONNECTED => Err(RecvError),
            // The receiver must have been `Future::poll`ed prior to this call.
            #[cfg(feature = "async")]
            RECEIVING => panic!("{}", RECEIVER_USED_SYNC_AND_ASYNC_ERROR),
            _ => unreachable!(),
        }
    }

    /// Like [`Receiver::recv`], but will not block longer than `timeout`. Returns:
    ///  * `Ok(message)` if there was a message in the channel before the timeout was reached.
    ///  * `Err(Timeout)` if no message arrived on the channel before the timeout was reached.
    ///  * `Err(Disconnected)` if the sender was dropped before sending anything or if the message
    ///    has already been extracted by a previous receive call.
    ///
    /// If a message is returned, the channel is disconnected and any subsequent receive operation
    /// using this receiver will return an error.
    ///
    /// If the supplied `timeout` is so large that Rust's `Instant` type can't represent this point
    /// in the future this falls back to an indefinitely blocking receive operation.
    ///
    /// # Panics
    ///
    /// Panics if called after this receiver has been polled asynchronously.
    #[cfg(feature = "std")]
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        match Instant::now().checked_add(timeout) {
            Some(deadline) => self.recv_deadline(deadline),
            None => self.recv_ref().map_err(|_| RecvTimeoutError::Disconnected),
        }
    }

    /// Like [`Receiver::recv`], but will not block longer than until `deadline`. Returns:
    ///  * `Ok(message)` if there was a message in the channel before the deadline was reached.
    ///  * `Err(Timeout)` if no message arrived on the channel before the deadline was reached.
    ///  * `Err(Disconnected)` if the sender was dropped before sending anything or if the message
    ///    has already been extracted by a previous receive call.
    ///
    /// If a message is returned, the channel is disconnected and any subsequent receive operation
    /// using this receiver will return an error.
    ///
    /// # Panics
    ///
    /// Panics if called after this receiver has been polled asynchronously.
    #[cfg(feature = "std")]
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        let channel_ptr = self.channel_ptr;
        let channel = unsafe { channel_ptr.as_ref() };

        match channel.state.load(SeqCst) {
            // The sender is alive but has not sent anything yet. We prepare to park.
            EMPTY => {
                // Conditionally add a delay here to help the tests trigger the edge cases where
                // the sender manages to be dropped or send something before we are able to store
                // our waker object in the channel.
                #[cfg(oneshot_test_delay)]
                std::thread::sleep(std::time::Duration::from_millis(10));

                // Write our thread instance to the channel.
                unsafe { channel.write_waker(ReceiverWaker::current_thread()) };

                match channel
                    .state
                    .compare_exchange(EMPTY, RECEIVING, SeqCst, SeqCst)
                {
                    // We stored our waker, now we park until the sender has changed the state
                    Ok(EMPTY) => loop {
                        let (state, timed_out) = if let Some(timeout) =
                            deadline.checked_duration_since(Instant::now())
                        {
                            thread::park_timeout(timeout);
                            (channel.state.load(SeqCst), false)
                        } else {
                            // We reached the deadline. Stop being in the receiving state.
                            (channel.state.swap(EMPTY, SeqCst), true)
                        };
                        match state {
                            // The sender sent the message while we were parked.
                            MESSAGE => {
                                channel.state.store(DISCONNECTED, SeqCst);
                                break Ok(unsafe { channel.take_message() });
                            }
                            // The sender was dropped while we were parked.
                            DISCONNECTED => break Err(RecvTimeoutError::Disconnected),
                            // State did not change, spurious wakeup, park again.
                            RECEIVING => {
                                if timed_out {
                                    unsafe { channel.drop_waker() };
                                    break Err(RecvTimeoutError::Timeout);
                                }
                            }
                            _ => unreachable!(),
                        }
                    },
                    // The sender sent the message while we prepared to park.
                    Err(MESSAGE) => {
                        channel.state.store(DISCONNECTED, SeqCst);
                        unsafe { channel.drop_waker() };
                        Ok(unsafe { channel.take_message() })
                    }
                    // The sender was dropped before sending anything while we prepared to park.
                    Err(DISCONNECTED) => {
                        unsafe { channel.drop_waker() };
                        Err(RecvTimeoutError::Disconnected)
                    }
                    _ => unreachable!(),
                }
            }
            // The sender sent the message.
            MESSAGE => {
                channel.state.store(DISCONNECTED, SeqCst);
                Ok(unsafe { channel.take_message() })
            }
            // The sender was dropped before sending anything, or we already received the message.
            DISCONNECTED => Err(RecvTimeoutError::Disconnected),
            // The receiver must have been `Future::poll`ed prior to this call.
            #[cfg(feature = "async")]
            RECEIVING => panic!("{}", RECEIVER_USED_SYNC_AND_ASYNC_ERROR),
            _ => unreachable!(),
        }
    }
}

#[cfg(feature = "async")]
impl<T> core::future::Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let channel = unsafe { self.channel_ptr.as_ref() };

        match channel.state.load(SeqCst) {
            // The sender is alive but has not sent anything yet.
            EMPTY => unsafe { channel.write_async_waker(cx) },
            // We were polled again while waiting for the sender. Replace the waker with the new one.
            RECEIVING => {
                match channel
                    .state
                    .compare_exchange(RECEIVING, EMPTY, SeqCst, SeqCst)
                {
                    // We successfully changed the state back to EMPTY. Replace the waker.
                    Ok(RECEIVING) => {
                        unsafe { channel.drop_waker() };
                        unsafe { channel.write_async_waker(cx) }
                    }
                    // The sender sent the message while we prepared to replace the waker.
                    // We take the message and mark the channel disconnected.
                    // The sender has already taken the waker.
                    Err(MESSAGE) => {
                        channel.state.store(DISCONNECTED, SeqCst);
                        Poll::Ready(Ok(unsafe { channel.take_message() }))
                    }
                    // The sender was dropped before sending anything while we prepared to park.
                    // The sender has taken the waker already.
                    Err(DISCONNECTED) => Poll::Ready(Err(RecvError)),
                    _ => unreachable!(),
                }
            }
            // The sender sent the message.
            MESSAGE => {
                channel.state.store(DISCONNECTED, SeqCst);
                Poll::Ready(Ok(unsafe { channel.take_message() }))
            }
            // The sender was dropped before sending anything, or we already received the message.
            DISCONNECTED => Poll::Ready(Err(RecvError)),
            _ => unreachable!(),
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // SAFETY: The reference won't be used after it is freed in this method
        let channel = unsafe { self.channel_ptr.as_ref() };

        // Set the channel state to disconnected and read what state the receiver was in
        match channel.state.swap(DISCONNECTED, SeqCst) {
            // The sender has not sent anything, nor is it dropped.
            EMPTY => (),
            // The sender already sent something. We must drop it, and free the channel.
            MESSAGE => {
                unsafe { channel.drop_message() };
                unsafe { dealloc(self.channel_ptr) };
            }
            // The receiver has been polled.
            #[cfg(feature = "async")]
            RECEIVING => {
                unsafe { channel.drop_waker() };
            }
            // The sender was already dropped. We are responsible for freeing the channel.
            DISCONNECTED => {
                unsafe { dealloc(self.channel_ptr) };
            }
            _ => unreachable!(),
        }
    }
}

/// All the values that the `Channel::state` field can have during the lifetime of a channel.
mod states {
    /// The initial channel state. Active while both endpoints are still alive, no message has been
    /// sent, and the receiver is not receiving.
    pub const EMPTY: u8 = 0;
    /// A message has been sent to the channel, but the receiver has not yet read it.
    pub const MESSAGE: u8 = 1;
    /// No message has yet been sent on the channel, but the receiver is currently receiving.
    pub const RECEIVING: u8 = 2;
    /// The channel has been closed. This means that either the sender or receiver has been dropped,
    /// or the message sent to the channel has already been received. Since this is a oneshot
    /// channel, it is disconnected after the one message it is supposed to hold has been
    /// transmitted.
    pub const DISCONNECTED: u8 = 3;
}
use states::*;

/// Internal channel data structure structure. the `channel` method allocates and puts one instance
/// of this struct on the heap for each oneshot channel instance. The struct holds:
/// * The current state of the channel.
/// * The message in the channel. This memory is uninitialized until the message is sent.
/// * The waker instance for the thread or task that is currently receiving on this channel.
///   This memory is uninitialized until the receiver starts receiving.
struct Channel<T> {
    state: AtomicU8,
    message: UnsafeCell<MaybeUninit<T>>,
    waker: UnsafeCell<MaybeUninit<ReceiverWaker>>,
}

impl<T> Channel<T> {
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(EMPTY),
            message: UnsafeCell::new(MaybeUninit::uninit()),
            waker: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    #[inline(always)]
    unsafe fn message(&self) -> &MaybeUninit<T> {
        #[cfg(loom)]
        {
            self.message.with(|ptr| &*ptr)
        }

        #[cfg(not(loom))]
        {
            &*self.message.get()
        }
    }

    #[inline(always)]
    unsafe fn with_message_mut<F>(&self, op: F)
    where
        F: FnOnce(&mut MaybeUninit<T>),
    {
        #[cfg(loom)]
        {
            self.message.with_mut(|ptr| op(&mut *ptr))
        }

        #[cfg(not(loom))]
        {
            op(&mut *self.message.get())
        }
    }

    #[inline(always)]
    #[cfg(any(feature = "std", feature = "async"))]
    unsafe fn with_waker_mut<F>(&self, op: F)
    where
        F: FnOnce(&mut MaybeUninit<ReceiverWaker>),
    {
        #[cfg(loom)]
        {
            self.waker.with_mut(|ptr| op(&mut *ptr))
        }

        #[cfg(not(loom))]
        {
            op(&mut *self.waker.get())
        }
    }

    #[inline(always)]
    unsafe fn write_message(&self, message: T) {
        self.with_message_mut(|slot| slot.as_mut_ptr().write(message));
    }

    #[inline(always)]
    unsafe fn take_message(&self) -> T {
        #[cfg(loom)]
        {
            self.message.with(|ptr| ptr::read(ptr)).assume_init()
        }

        #[cfg(not(loom))]
        {
            ptr::read(self.message.get()).assume_init()
        }
    }

    #[inline(always)]
    unsafe fn drop_message(&self) {
        self.with_message_mut(|slot| slot.assume_init_drop());
    }

    #[cfg(any(feature = "std", feature = "async"))]
    #[inline(always)]
    unsafe fn write_waker(&self, waker: ReceiverWaker) {
        self.with_waker_mut(|slot| slot.as_mut_ptr().write(waker));
    }

    #[inline(always)]
    unsafe fn take_waker(&self) -> ReceiverWaker {
        #[cfg(loom)]
        {
            self.waker.with(|ptr| ptr::read(ptr)).assume_init()
        }

        #[cfg(not(loom))]
        {
            ptr::read(self.waker.get()).assume_init()
        }
    }

    #[cfg(any(feature = "std", feature = "async"))]
    #[inline(always)]
    unsafe fn drop_waker(&self) {
        self.with_waker_mut(|slot| slot.assume_init_drop());
    }

    #[cfg(feature = "async")]
    unsafe fn write_async_waker(&self, cx: &mut task::Context<'_>) -> Poll<Result<T, RecvError>> {
        // Write our thread instance to the channel.
        self.write_waker(ReceiverWaker::task_waker(cx));

        match self
            .state
            .compare_exchange(EMPTY, RECEIVING, SeqCst, SeqCst)
        {
            // We stored our waker, now we return and let the sender wake us up
            Ok(EMPTY) => Poll::Pending,
            // The sender was dropped before sending anything while we prepared to park.
            Err(DISCONNECTED) => {
                self.drop_waker();
                Poll::Ready(Err(RecvError))
            }
            // The sender sent the message while we prepared to park.
            // We take the message and mark the channel disconnected.
            Err(MESSAGE) => {
                self.drop_waker();
                self.state.store(DISCONNECTED, SeqCst);
                Poll::Ready(Ok(self.take_message()))
            }
            _ => unreachable!(),
        }
    }
}

enum ReceiverWaker {
    /// The receiver is waiting synchronously. Its thread is parked.
    #[cfg(feature = "std")]
    Thread(thread::Thread),
    /// The receiver is waiting asynchronously. Its task can be woken up with this `Waker`.
    #[cfg(feature = "async")]
    Task(task::Waker),
    /// A little hack to not make this enum an uninhibitable type when no features are enabled.
    #[cfg(not(any(feature = "async", feature = "std")))]
    _Uninhabited,
}

impl ReceiverWaker {
    #[cfg(feature = "std")]
    pub fn current_thread() -> Self {
        Self::Thread(thread::current())
    }

    #[cfg(feature = "async")]
    pub fn task_waker(cx: &task::Context<'_>) -> Self {
        Self::Task(cx.waker().clone())
    }

    pub fn unpark(self) {
        match self {
            #[cfg(feature = "std")]
            ReceiverWaker::Thread(thread) => thread.unpark(),
            #[cfg(feature = "async")]
            ReceiverWaker::Task(waker) => waker.wake(),
            #[cfg(not(any(feature = "async", feature = "std")))]
            ReceiverWaker::_Uninhabited => unreachable!(),
        }
    }
}

#[cfg(not(loom))]
#[test]
fn receiver_waker_size() {
    let expected: usize = match (cfg!(feature = "std"), cfg!(feature = "async")) {
        (false, false) => 0,
        (false, true) => 16,
        (true, false) => 8,
        (true, true) => 24,
    };
    assert_eq!(mem::size_of::<ReceiverWaker>(), expected);
}

#[cfg(all(feature = "std", feature = "async"))]
const RECEIVER_USED_SYNC_AND_ASYNC_ERROR: &str =
    "Invalid to call a blocking receive method on oneshot::Receiver after it has been polled";

#[inline]
pub(crate) unsafe fn dealloc<T>(channel: NonNull<Channel<T>>) {
    drop(Box::from_raw(channel.as_ptr()))
}
