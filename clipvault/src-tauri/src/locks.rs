//! Poison-tolerant locking. A panic while a lock is held poisons it, and a
//! plain `lock().unwrap()` would then panic in every later user, turning one
//! bug into a dead save path. The data behind these locks stays consistent
//! (plain values replaced in one assignment), so the guard is recovered
//! instead.
use std::sync::{
    Condvar, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use std::time::Duration;

pub fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn read<T: ?Sized>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}

pub fn write<T: ?Sized>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}

/// `Condvar::wait_timeout` that recovers a poisoned guard; the timeout flag is
/// dropped because every caller re-checks its own condition in a loop.
pub fn wait_timeout<'a, T>(
    cv: &Condvar,
    guard: MutexGuard<'a, T>,
    dur: Duration,
) -> MutexGuard<'a, T> {
    cv.wait_timeout(guard, dur)
        .unwrap_or_else(PoisonError::into_inner)
        .0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_locks_are_recovered() {
        let m = Mutex::new(1);
        let l = RwLock::new(2);
        std::thread::scope(|s| {
            let _ = s
                .spawn(|| {
                    let _g = m.lock().unwrap();
                    let _w = l.write().unwrap();
                    panic!("poison");
                })
                .join();
        });
        assert!(m.is_poisoned() && l.is_poisoned());
        *lock(&m) += 1;
        *write(&l) += 1;
        assert_eq!((*lock(&m), *read(&l)), (2, 3));
    }

    #[test]
    fn poisoned_condvar_wait_is_recovered() {
        let m = Mutex::new(0);
        let cv = Condvar::new();
        std::thread::scope(|s| {
            let _ = s
                .spawn(|| {
                    let _g = m.lock().unwrap();
                    panic!("poison");
                })
                .join();
        });
        assert!(m.is_poisoned());
        let mut g = wait_timeout(&cv, lock(&m), Duration::from_millis(1));
        *g += 1;
        drop(g);
        assert_eq!(*lock(&m), 1);
    }
}
