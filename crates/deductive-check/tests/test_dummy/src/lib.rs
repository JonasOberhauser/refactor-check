pub fn regular_function(x: i32) -> i32 {
    x + 1
}

pub fn with_unsafe_block(x: *const i32) -> i32 {
    unsafe { *x }
}

pub fn with_assertion(x: i32) -> i32 {
    assert!(x > 0);
    x
}

/// # Guarantees
/// Always returns 42.
pub fn with_docs_guarantee(x: i32) -> i32 {
    let _ = x;
    42
}

#[test]
fn test_direct() {
    assert_eq!(regular_function(1), 2);
}

#[tokio::test]
async fn test_tokio() {
    assert_eq!(regular_function(1), 2);
}

#[test_log::test(tokio::test)]
async fn test_log_tokio() {
    assert_eq!(regular_function(1), 2);
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_in_cfg_test_mod() {
        assert!(true);
    }

    #[tokio::test]
    async fn tokio_in_cfg_test_mod() {
        assert!(true);
    }
}

pub struct Counter {
    value: i32,
}

impl Counter {
    pub fn increment(&mut self) {
        self.value += 1;
    }

    pub unsafe fn raw_ptr(&self) -> *const i32 {
        &self.value
    }
}

pub trait Processor {
    fn process(&self, x: i32) -> i32;
}

impl Processor for Counter {
    fn process(&self, x: i32) -> i32 {
        self.value + x
    }
}

/// Does a pointer read.
///
/// # Safety
///
/// The pointer must be valid and aligned.
pub unsafe fn read_ptr(x: *const i32) -> i32 {
    unsafe { *x }
}

/// A safe wrapper around pointer read.
///
/// # Guarantees
///
/// Returns the value behind the pointer without UB.
pub fn safe_read(x: &i32) -> i32 {
    unsafe { read_ptr(x) }
}

#[doc = "Doc attribute function."]
#[doc = "# Ensures"]
#[doc = "result >= 0"]
pub fn with_doc_attr(x: i32) -> i32 {
    let _ = x;
    0
}

/// # Preconditions
///
/// `x` must be positive.
pub fn with_preconditions(x: i32) -> i32 {
    assert!(x > 0);
    x
}

pub fn calls_precondition_fn(x: i32) -> i32 {
    with_preconditions(x)
}

#[cfg(feature = "verification")]
pub fn verification_only() -> i32 {
    99
}
