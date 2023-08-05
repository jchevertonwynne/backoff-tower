# backoff-tower

A tower layer to apply a backoff strategy to retried requests

## Overview

`tower` has a builtin `RetryLayer` which retries requests immediately. 
`BackoffLayer` is a wrapper around a `RetryLayer` that applies a `BackoffStrategy` to requests
so that there is a delay before they are retried. Both `tokio` and `async-std` are supported, with `tokio` 
being selected via default features

```rust
#[derive(Clone)]
struct MyPolicy {
    attempts_left: usize,
}

impl Policy<usize, usize, &'static str> for MyPolicy {
    type Future = Ready<Self>;

    fn retry(&self, _req: &usize, result: Result<&usize, &&'static str>) -> Option<Self::Future> {
        if self.attempts_left == 0 {
            return None;
        }

        match result {
            Ok(_) => None,
            Err(_) => Some(ready(MyPolicy { attempts_left: self.attempts_left - 1 }))
        }
    }

    fn clone_request(&self, req: &usize) -> Option<usize> {
        Some(req + 1)
    }
}

#[tokio::main]
async fn main() {
    let mut service = ServiceBuilder::new()
        .layer(BackoffLayer::new(MyPolicy { attempts_left: 8 }, ExponentialBackoffStrategy))
        .service_fn(|x: usize| async move {
            if x % 10 == 0 {
                Ok(x / 10)
            } else {
                Err("bad input")
            }
        });

    assert_eq!(Ok(6), service.call(55).await, "should be the next multiple of 10 divided by 10");
    assert_eq!(Err("bad input"), service.call(51).await, "retry limit should have been hit");
}

```