use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::time::Duration;
use tower::retry::future::ResponseFuture;
use tower::retry::{Policy, Retry, RetryLayer};
use tower::{Layer, Service};

/// A layer that creates a service that will attempt & reattempt to perform a service call based on a policy.
///
/// Each subsequent call will have a backoff period as defined by the passed strategy
pub struct BackoffLayer<P, B> {
    retry: RetryLayer<BackoffPolicy<P>>,
    backoff: B,
}

impl<P, B> BackoffLayer<P, B> {
    pub fn new(policy: P, backoff_strategy: B) -> Self {
        BackoffLayer {
            retry: RetryLayer::new(BackoffPolicy::new(policy)),
            backoff: backoff_strategy,
        }
    }
}

impl<S, P, B> Layer<S> for BackoffLayer<P, B>
where
    P: Clone,
    B: Clone,
{
    type Service = BackoffService<P, S, B>;

    fn layer(&self, inner: S) -> Self::Service {
        BackoffService::new_from_retry(self.retry.layer(BackoffInnerService {
            inner,
            backoff: self.backoff.clone(),
        }))
    }
}

/// A service for the retrying of a call with back offs.
///
/// This service adds the backoff wrapper to the request
/// so that the inner service can choose an appropriate
/// backoff period before reattempting its service call
#[derive(Clone)]
pub struct BackoffService<P, S, B> {
    backoff_retry: Retry<BackoffPolicy<P>, BackoffInnerService<S, B>>,
}

impl<P, S, B> BackoffService<P, S, B> {
    pub fn new(policy: P, inner: S, backoff: B) -> Self {
        BackoffService {
            backoff_retry: Retry::new(
                BackoffPolicy::new(policy),
                BackoffInnerService::new(inner, backoff),
            ),
        }
    }

    pub fn new_from_retry(retry: Retry<BackoffPolicy<P>, BackoffInnerService<S, B>>) -> Self {
        BackoffService {
            backoff_retry: retry,
        }
    }
}

impl<P, S, B, Req> Service<Req> for BackoffService<P, S, B>
where
    P: Policy<Req, S::Response, S::Error> + Clone,
    B: BackoffStrategy,
    S: Service<Req> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<BackoffPolicy<P>, BackoffInnerService<S, B>, BackoffRequest<Req>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.backoff_retry.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.backoff_retry.call(BackoffRequest::new(req))
    }
}

/// The inner service which performs the backed off request
///
/// Unwraps the request from the backoff wrapper & applies
/// a backoff period to the future as necessary
#[derive(Debug, Clone)]
pub struct BackoffInnerService<S, B> {
    inner: S,
    backoff: B,
}

impl<S, B> BackoffInnerService<S, B> {
    fn new(inner: S, backoff: B) -> Self {
        BackoffInnerService { inner, backoff }
    }
}

impl<S, B, Req> Service<BackoffRequest<Req>> for BackoffInnerService<S, B>
where
    S: Service<Req>,
    B: BackoffStrategy,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BackoffFut<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: BackoffRequest<Req>) -> Self::Future {
        let BackoffRequest { calls, req } = req;
        let backoff = self.backoff.backoff_duration(calls);
        let is_first_call = calls == 0;
        BackoffFut::new(is_first_call, backoff, self.inner.call(req))
    }
}

#[cfg(feature = "tokio")]
pin_project! {
    /// A future with a sleep before it can be polled
    pub struct BackoffFut<F> {
        slept: bool,
        #[pin]
        sleep: tokio::time::Sleep,
        #[pin]
        fut: F,
    }
}

#[cfg(feature = "async_std")]
pin_project! {
    /// A future with a sleep before it can be polled
    pub struct BackoffFut<F> {
        slept: bool,
        #[pin]
        sleep: Pin<Box<dyn Future<Output=()>>>,
        #[pin]
        fut: F,
    }
}

impl<F> BackoffFut<F> {
    fn new(slept: bool, duration: Duration, fut: F) -> Self {
        BackoffFut {
            slept,
            #[cfg(feature = "tokio")]
            sleep: tokio::time::sleep(duration),
            #[cfg(feature = "async_std")]
            sleep: Box::pin(async_std::task::sleep(duration)),
            fut,
        }
    }
}

impl<F> Future for BackoffFut<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if !*this.slept {
            ready!(this.sleep.poll(cx));
            *this.slept = true;
        }

        this.fut.poll(cx)
    }
}

/// A policy which wraps a policy for the raw request type
#[derive(Debug, Clone)]
pub struct BackoffPolicy<P> {
    inner: P,
}

impl<P> BackoffPolicy<P> {
    fn new(policy: P) -> Self {
        Self { inner: policy }
    }
}

pin_project! {
    pub struct IntoBackoffPolicyFut<F> {
        #[pin]
        fut: F
    }
}

impl<F> IntoBackoffPolicyFut<F> {
    fn new(fut: F) -> Self {
        IntoBackoffPolicyFut { fut }
    }
}

impl<F> Future for IntoBackoffPolicyFut<F>
where
    F: Future,
{
    type Output = BackoffPolicy<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let res = ready!(this.fut.poll(cx));
        Poll::Ready(BackoffPolicy::new(res))
    }
}

/// Policy for a backed off request defers to the policy for the raw request
/// and updates the calls count upon clone
impl<P, Req, Res, Err> Policy<BackoffRequest<Req>, Res, Err> for BackoffPolicy<P>
where
    P: Policy<Req, Res, Err>,
{
    type Future = IntoBackoffPolicyFut<P::Future>;

    fn retry(&self, req: &BackoffRequest<Req>, result: Result<&Res, &Err>) -> Option<Self::Future> {
        let BackoffRequest { req, .. } = req;
        self.inner.retry(req, result).map(IntoBackoffPolicyFut::new)
    }

    fn clone_request(&self, req: &BackoffRequest<Req>) -> Option<BackoffRequest<Req>> {
        let BackoffRequest { calls, req } = req;
        self.inner
            .clone_request(req)
            .map(|req| BackoffRequest::new_with_calls(req, calls + 1))
    }
}

/// Request wrapper to track the number of retries of the request
pub struct BackoffRequest<R> {
    // 4bn is hopefully enough 🤞
    calls: u32,
    req: R,
}

impl<R> BackoffRequest<R> {
    fn new(req: R) -> Self {
        BackoffRequest { calls: 0, req }
    }

    fn new_with_calls(req: R, calls: u32) -> Self {
        BackoffRequest { calls, req }
    }
}

/// A trait describing how long to backoff for each subsequent attempt
pub trait BackoffStrategy: Clone {
    fn backoff_duration(&self, repeats: u32) -> Duration;
}

pub mod backoff_strategies {
    use crate::BackoffStrategy;
    use std::time::Duration;

    /// Performs backoffs in millisecond powers of 2
    #[derive(Debug, Clone)]
    pub struct ExponentialBackoffStrategy;

    impl BackoffStrategy for ExponentialBackoffStrategy {
        fn backoff_duration(&self, repeats: u32) -> Duration {
            Duration::from_millis(1 << repeats)
        }
    }

    /// Performs backoffs in fibonacci milliseconds
    #[derive(Debug, Clone)]
    pub struct FibonacciBackoffStrategy;

    impl BackoffStrategy for FibonacciBackoffStrategy {
        fn backoff_duration(&self, repeats: u32) -> Duration {
            let mut a = 0;
            let mut b = 1;
            for _ in 0..repeats {
                let c = a + b;
                a = b;
                b = c;
            }
            Duration::from_millis(a)
        }
    }

    /// Performs backoffs in multiples of a duration
    #[derive(Debug, Clone)]
    pub struct LinearBackoffStrategy {
        duration_multiple: Duration,
    }

    impl LinearBackoffStrategy {
        pub fn new(duration_multiple: Duration) -> Self {
            Self { duration_multiple }
        }
    }

    impl BackoffStrategy for LinearBackoffStrategy {
        fn backoff_duration(&self, repeats: u32) -> Duration {
            self.duration_multiple * repeats
        }
    }

    /// Backoff is a constant value
    #[derive(Debug, Clone)]
    pub struct ConstantBackoffStrategy {
        duration: Duration,
    }

    impl ConstantBackoffStrategy {
        pub fn new(duration: Duration) -> Self {
            Self { duration }
        }
    }

    impl BackoffStrategy for ConstantBackoffStrategy {
        fn backoff_duration(&self, _repeats: u32) -> Duration {
            self.duration
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::backoff_layer::{BackoffInnerService, BackoffRequest};
    use crate::backoff_strategies::ExponentialBackoffStrategy;
    use crate::BackoffLayer;
    use std::error::Error;
    use std::future::{ready, Ready};
    use tokio::select;
    use tower::retry::Policy;
    use tower::{Service, ServiceBuilder};

    #[derive(Clone)]
    struct MyPolicy {
        attempts_left: usize,
    }

    impl Policy<usize, usize, &'static str> for MyPolicy {
        type Future = Ready<Self>;

        fn retry(
            &self,
            _req: &usize,
            result: Result<&usize, &&'static str>,
        ) -> Option<Self::Future> {
            if self.attempts_left == 0 {
                return None;
            }

            match result {
                Ok(_) => None,
                Err(_) => Some(ready(MyPolicy {
                    attempts_left: self.attempts_left - 1,
                })),
            }
        }

        fn clone_request(&self, req: &usize) -> Option<usize> {
            Some(req + 1)
        }
    }

    #[tokio::test]
    async fn retries_work() -> Result<(), Box<dyn Error>> {
        let mut service = ServiceBuilder::new()
            .layer(BackoffLayer::new(
                MyPolicy { attempts_left: 4 },
                ExponentialBackoffStrategy,
            ))
            .service_fn(|x: usize| async move {
                if x % 10 == 0 {
                    Ok(x / 10)
                } else {
                    Err("bad input")
                }
            });

        assert_eq!(
            Ok(6),
            service.call(60).await,
            "should be the next multiple of 10 divided by 10"
        );
        assert_eq!(
            Ok(6),
            service.call(59).await,
            "should be the next multiple of 10 divided by 10"
        );
        assert_eq!(
            Ok(6),
            service.call(58).await,
            "should be the next multiple of 10 divided by 10"
        );
        assert_eq!(
            Ok(6),
            service.call(57).await,
            "should be the next multiple of 10 divided by 10"
        );
        assert_eq!(
            Ok(6),
            service.call(56).await,
            "should be the next multiple of 10 divided by 10"
        );
        assert_eq!(
            Err("bad input"),
            service.call(55).await,
            "should error as ran out of retries"
        );

        Ok(())
    }

    #[tokio::test]
    async fn subsequent_retires_have_different_wait_periods() -> Result<(), Box<dyn Error>> {
        let mut backoff_inner_svc = BackoffInnerService::new(
            tower::service_fn(|x: usize| async move {
                if x % 10 == 0 {
                    Ok(x / 10)
                } else {
                    Err("bad input")
                }
            }),
            ExponentialBackoffStrategy,
        );

        assert_eq!(6, backoff_inner_svc.call(BackoffRequest::new(60)).await?);

        let a = backoff_inner_svc.call(BackoffRequest::new(60));
        let b = backoff_inner_svc.call(BackoffRequest::new_with_calls(60, 1));
        let c = backoff_inner_svc.call(BackoffRequest::new_with_calls(60, 2));

        assert!(a.slept, "0 calls should have no backoff");
        assert!(!b.slept, "1 or more calls should have backoffs");
        assert!(!c.slept, "1 or more calls should have backoffs");

        #[cfg(feature = "tokio")]
        assert!(b.sleep.deadline() < c.sleep.deadline());

        select! {
            _ = b => {}
            _ = c => {
                panic!("call b should respond first due to a smaller backoff")
            }
        }

        Ok(())
    }
}
