use vardr::store::Store;

struct ForkedChild(libc::pid_t);
impl Drop for ForkedChild {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
            libc::waitpid(self.0, std::ptr::null_mut(), 0);
        }
    }
}

#[test]
fn dropping_a_lock_releases_it_while_a_child_inherits_its_descriptor() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().canonicalize().unwrap().join("data")).unwrap();
    let lock = store.lock("test").unwrap();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        // Only async-signal-safe calls after fork; never enter the Rust runtime.
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    let _child = ForkedChild(pid);
    assert!(store.lock("test").is_err());
    drop(lock);
    // The child still holds its descriptor. Explicit unlock must release flock now.
    let _reacquired = store.lock("test").unwrap();
}
