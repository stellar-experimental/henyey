#![allow(dead_code)]
enum State { Idle, A(P), B(Q) }
struct P { h: H }
struct Q { h: H }
struct H;
impl core::future::Future for H { type Output = (); fn poll(self: core::pin::Pin<&mut Self>, _cx: &mut core::task::Context<'_>) -> core::task::Poll<()> { core::task::Poll::Ready(()) } }
struct C { state: State }
enum E { A, B }
impl C {
    async fn poll_completion(&mut self) -> E {
        if matches!(self.state, State::Idle) {
            return std::future::pending().await;
        }
        let was_a = matches!(self.state, State::A(_));
        if was_a {
            let _jr = match &mut self.state {
                State::A(p) => (&mut p.h).await,
                _ => unreachable!(),
            };
            match std::mem::replace(&mut self.state, State::Idle) {
                State::A(_p) => E::A,
                _ => unreachable!(),
            }
        } else {
            let _jr = match &mut self.state {
                State::B(p) => (&mut p.h).await,
                _ => unreachable!(),
            };
            match std::mem::replace(&mut self.state, State::Idle) {
                State::B(_p) => E::B,
                _ => unreachable!(),
            }
        }
    }
}
fn main() {}
