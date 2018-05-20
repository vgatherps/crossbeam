use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

use internal::context::{self, Context};
use internal::select::CaseId;

/// A selection case, identified by a `Context` and a `CaseId`.
///
/// Note that multiple threads could be operating on a single channel end, as well as a single
/// thread on multiple different channel ends.
pub struct Case {
    /// A context associated with the thread owning this case.
    pub context: Arc<Context>,

    /// The case ID.
    pub case_id: CaseId,

    pub packet: usize,
}

/// A simple wait queue for list-based and array-based channels.
///
/// This data structure is used for registering selection cases before blocking and waking them
/// up when the channel receives a message, sends one, or gets closed.
pub struct Waker {
    /// The list of registered selection cases.
    cases: Mutex<VecDeque<Case>>,

    /// Number of cases in the list.
    len: AtomicUsize,
}

// TODO: inline everything?
impl Waker {
    /// Creates a new `Waker`.
    #[inline]
    pub fn new() -> Self {
        Waker {
            cases: Mutex::new(VecDeque::new()),
            len: AtomicUsize::new(0),
        }
    }

    /// Registers the current thread with `case_id`.
    pub fn register(&self, case_id: CaseId) {
        let mut cases = self.cases.lock();
        cases.push_back(Case {
            context: context::current(),
            case_id,
            packet: 0,
        });
        self.len.store(cases.len(), Ordering::SeqCst);
    }

    pub fn register_with_packet(&self, case_id: CaseId, packet: usize) {
        let mut cases = self.cases.lock();
        cases.push_back(Case {
            context: context::current(),
            case_id,
            packet,
        });
        self.len.store(cases.len(), Ordering::SeqCst);
    }

    /// Unregisters the current thread with `case_id`.
    pub fn unregister(&self, case_id: CaseId) -> Option<Case> {
        if self.len.load(Ordering::SeqCst) > 0 {
            let mut cases = self.cases.lock();

            if let Some((i, _)) = cases.iter().enumerate().find(|&(_, case)| case.case_id == case_id) {
                let case = cases.remove(i);
                self.len.store(cases.len(), Ordering::SeqCst);
                Self::maybe_shrink(&mut cases);
                case
            } else {
                None
            }
        } else {
            None
        }
    }

    #[inline]
    pub fn wake_one(&self) -> Option<Case> {
        if self.len.load(Ordering::SeqCst) > 0 {
            let thread_id = context::current_thread_id();
            let mut cases = self.cases.lock();

            for i in 0..cases.len() {
                if cases[i].context.thread.id() != thread_id {
                    if cases[i].context.try_select(cases[i].case_id, cases[i].packet) {
                        let case = cases.remove(i).unwrap();
                        self.len.store(cases.len(), Ordering::SeqCst);
                        Self::maybe_shrink(&mut cases);

                        drop(cases);
                        case.context.unpark();
                        return Some(case);
                    }
                }
            }
        }

        None
    }

    /// Aborts all currently registered selection cases.
    pub fn abort_all(&self) {
        if self.len.load(Ordering::SeqCst) > 0 {
            let mut cases = self.cases.lock();

            self.len.store(0, Ordering::SeqCst);
            for case in cases.drain(..) {
                if case.context.try_abort() {
                    case.context.unpark();
                }
            }

            Self::maybe_shrink(&mut cases);
        }
    }

    /// Returns `true` if there exists a case which isn't owned by the current thread.
    #[inline]
    pub fn can_notify(&self) -> bool {
        if self.len.load(Ordering::SeqCst) > 0 {
            let cases = self.cases.lock();
            let thread_id = context::current_thread_id();

            for i in 0..cases.len() {
                if cases[i].context.thread.id() != thread_id {
                    return true;
                }
            }
        }
        false
    }

    /// Shrinks the internal deque if it's capacity is much larger than length.
    fn maybe_shrink(cases: &mut VecDeque<Case>) {
        if cases.capacity() > 32 && cases.len() < cases.capacity() / 4 {
            let mut v = VecDeque::with_capacity(cases.capacity() / 2);
            v.extend(cases.drain(..));
            *cases = v;
        }
    }
}

impl Drop for Waker {
    fn drop(&mut self) {
        debug_assert!(self.cases.lock().is_empty());
        debug_assert_eq!(self.len.load(Ordering::SeqCst), 0);
    }
}
