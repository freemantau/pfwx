use crate::{
    reactor::{AliveState, SyncObject}, retcode
};
use pbni::{pbx::*, prelude::*};

struct AsyncTest {
    session: Session,
    ctx: ContextObject,
    alive: reactor::AliveState,
    inner: Option<AsyncTestInner>
}

#[nonvisualobject(name = "n_async_test")]
impl AsyncTest {
    #[constructor]
    fn new(session: Session, ctx: ContextObject) -> Self {
        AsyncTest {
            session,
            ctx,
            alive: reactor::AliveState::new(),
            inner: None
        }
    }

    #[method]
    fn version(&self) -> String { String::from("1.0") }

    #[method]
    fn copyright(&self) -> String { String::from(env!("CARGO_PKG_AUTHORS")) }

    #[method]
    fn async_call(&mut self) -> pblong {
        self.spawn(
            async {
                //tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                //"time is done".to_owned()
                Ok(reqwest::get("http://www.baidu.com").await?.text().await?)
            },
            |this, param: reqwest::Result<String>| {
                this.on_async(format!("{:?}", param)).unwrap();
            }
        );
        retcode::OK
    }

    #[event(name = "onAsync")]
    fn on_async(&mut self, param: String) -> Result<()> {}
}

impl AsyncTest {
    fn context_mut(&mut self) -> &mut ContextObject { &mut self.ctx }
}

impl SyncObject for AsyncTest {
    fn session(&self) -> &Session { &self.session }
    fn alive(&self) -> &AliveState { &self.alive }
}

#[allow(dead_code)]
struct AsyncTestInner {}
