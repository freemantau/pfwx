use super::{
    context::{Dispatcher, SyncContext}, runtime, UnsafeBox, UnsafePointer
};
use futures_util::FutureExt;
use pbni::pbx::Session;
use std::{
    future::Future, marker::PhantomData, panic::AssertUnwindSafe, sync::{Arc, Mutex, Weak}
};
use tokio::sync::oneshot;

/// 回调处理对象抽象
pub trait Handler: Sized + 'static {
    /// PB会话
    fn session(&self) -> &Session;

    /// 对象状态
    fn state(&self) -> &HandlerState;

    /// 对象回调派发器
    fn dispatcher(&self) -> HandlerDispatcher<Self> { HandlerDispatcher::bind(self) }

    /// 启动一个异步任务
    ///
    /// # Parameters
    ///
    /// - `fut` 异步任务
    ///
    /// # Returns
    ///
    /// `CancelHandle` 任务取消句柄
    ///
    /// # Cancellation
    ///
    /// - 通过`CancelHandle`手动取消
    /// - 此对象销毁时自动取消
    fn spawn<F>(&mut self, fut: F) -> CancelHandle
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static
    {
        let dispatcher = self.dispatcher();
        let (cancel_hdl, mut cancel_rx) = self.state().new_cancel_handle();

        //封装异步任务
        let fut = async move {
            tokio::pin! {
            let fut = AssertUnwindSafe(fut).catch_unwind();
            }
            loop {
                tokio::select! {
                    rv = &mut fut => {
                        if let Err(e) = rv {
                            let panic_info = match e.downcast_ref::<String>() {
                                Some(e) => &e,
                                None => {
                                    match e.downcast_ref::<&'static str>() {
                                        Some(e) => e,
                                        None => "unknown"
                                    }
                                },
                            };
                            dispatcher
                                .dispatch_panic(panic_info)
                                .await;
                        }
                        break;
                    },
                    _ = &mut cancel_rx => break,
                }
            }
        };

        //执行
        runtime::spawn(fut);

        cancel_hdl
    }

    /// 启动一个异步任务并接收执行结果
    ///
    /// # Parameters
    ///
    /// - `fut` 异步任务
    /// - `handler` 接收`fut`执行结果并在当前(UI)线程中执行
    ///
    /// # Returns
    ///
    /// `CancelHandle` 任务取消句柄
    ///
    /// # Cancellation
    ///
    /// - 通过`CancelHandle`手动取消
    /// - 此对象销毁时自动取消
    fn spawn_with_handler<F, H>(&mut self, fut: F, handler: H) -> CancelHandle
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
        H: Fn(&mut Self, F::Output) + Send + 'static
    {
        let dispatcher = self.dispatcher();
        let (cancel_hdl, mut cancel_rx) = self.state().new_cancel_handle();
        let handler = {
            let cancel_id = cancel_hdl.id();
            move |this: &mut Self, param: F::Output| {
                //删除取消ID成功说明任务没有被取消
                if this.state().remove_cancel_id(cancel_id) {
                    handler(this, param);
                }
            }
        };

        //封装异步任务
        let fut = async move {
            tokio::pin! {
            let fut = AssertUnwindSafe(fut).catch_unwind();
            }
            loop {
                tokio::select! {
                    rv = &mut fut => {
                        cancel_rx.close();
                        match rv {
                            Ok(rv) => {
                                //检查取消信号
                                if cancel_rx.try_recv().is_ok() {
                                    break;
                                }
                                dispatcher.dispatch_with_param(rv, handler).await;
                            },
                            Err(e) => {
                                let panic_info = match e.downcast_ref::<String>() {
                                    Some(e) => &e,
                                    None => {
                                        match e.downcast_ref::<&'static str>() {
                                            Some(e) => e,
                                            None => "unknown"
                                        }
                                    },
                                };
                                dispatcher
                                    .dispatch_panic(panic_info)
                                    .await;
                            }
                        }
                        break;
                    },
                    _ = &mut cancel_rx => break,
                }
            }
        };

        //执行
        runtime::spawn(fut);

        cancel_hdl
    }
}

/// 对象状态
#[derive(Default)]
pub struct HandlerState(Arc<Mutex<CancelManager>>);

impl HandlerState {
    pub fn new() -> HandlerState { Default::default() }

    /// 存活状态监视
    fn alive(&self) -> AliveState { AliveState(Arc::downgrade(&self.0)) }

    /// 新建一个异步任务取消句柄
    fn new_cancel_handle(&self) -> (CancelHandle, oneshot::Receiver<()>) {
        let mut inner = self.0.lock().unwrap();
        let (id, rx) = inner.new_cancel_id();
        drop(inner);
        (
            CancelHandle {
                id,
                state: Arc::downgrade(&self.0),
                _marker: PhantomData
            },
            rx
        )
    }

    /// 通过取消ID删除取消句柄
    fn remove_cancel_id(&self, id: u64) -> bool {
        let mut inner = self.0.lock().unwrap();
        inner.remove(id)
    }
}

/// 对象存活状态监视
#[derive(Clone)]
pub struct AliveState(Weak<Mutex<CancelManager>>);

impl AliveState {
    /// 是否存活
    pub fn is_alive(&self) -> bool { self.0.strong_count() != 0 }

    /// 是否死亡
    pub fn is_dead(&self) -> bool { self.0.strong_count() == 0 }
}

/// 异步任务取消管理器
#[derive(Default)]
struct CancelManager {
    next_id: u64,
    pending: Vec<(u64, oneshot::Sender<()>)>
}

impl CancelManager {
    /// 新建取消ID
    fn new_cancel_id(&mut self) -> (u64, oneshot::Receiver<()>) {
        let id = self.next_id;
        self.next_id += 1;
        let (tx, rx) = oneshot::channel();
        //优先覆盖失效的元素(任务Panic后残留)
        if let Some(idx) = self.pending.iter().position(|(_, tx)| tx.is_closed()) {
            self.pending[idx] = (id, tx);
        } else {
            self.pending.push((id, tx));
        }
        (id, rx)
    }

    /// 取消任务
    fn cancel(&mut self, id: u64) {
        if let Some(idx) = self.pending.iter().position(|item| item.0 == id) {
            let (_, tx) = self.pending.remove(idx);
            let _ = tx.send(());
        }
    }

    /// 删除取消通道
    fn remove(&mut self, id: u64) -> bool {
        let len = self.pending.len();
        self.pending.retain(|item| item.0 != id);
        len != self.pending.len()
    }
}

impl Drop for CancelManager {
    fn drop(&mut self) {
        //取消所有未完成的任务
        while let Some((_, tx)) = self.pending.pop() {
            let _ = tx.send(());
        }
    }
}

/// 异步任务取消句柄
#[derive(Clone)]
pub struct CancelHandle {
    id: u64,
    state: Weak<Mutex<CancelManager>>,
    // !Send
    _marker: PhantomData<*mut ()>
}

impl CancelHandle {
    /// 取消异步任务
    pub fn cancel(self) {
        if let Some(state) = self.state.upgrade() {
            let mut state = state.lock().unwrap();
            state.cancel(self.id);
        }
    }

    /// 取消ID
    fn id(&self) -> u64 { self.id }
}

/// 对象回调派发器
pub struct HandlerDispatcher<T> {
    dsp: Dispatcher,
    this: UnsafePointer<T>,
    alive: AliveState
}

impl<T> HandlerDispatcher<T>
where
    T: Handler
{
    /// 创建派发器并绑定对象
    fn bind(this: &T) -> Self {
        let sync_ctx = SyncContext::current(this.session());
        HandlerDispatcher {
            dsp: sync_ctx.dispatcher(),
            this: unsafe { UnsafePointer::from_raw(this as *const T as *mut T) },
            alive: this.state().alive()
        }
    }

    /// 发起回调请求给UI线程执行
    ///
    /// # Parameters
    ///
    /// - `handler` 回调过程在UI线程中执行
    ///
    /// # Returns
    ///
    /// 成功接收请求后返回`true`否则返回`false`
    pub async fn dispatch<H>(&self, handler: H) -> bool
    where
        H: FnOnce(&mut T) + Send + 'static
    {
        if self.alive.is_dead() {
            return false;
        }
        let handler = unsafe {
            let this = self.this.clone();
            Box::new(move |param: UnsafeBox<()>, alive: AliveState| {
                param.unpack();
                if alive.is_alive() {
                    let this = &mut *this.into_raw();
                    handler(this);
                }
            })
        };
        let param = unsafe { UnsafeBox::pack(()) };
        self.dsp.dispatch_invoke(param, handler, self.alive.clone()).await
    }

    /// 发起附带参数的回调请求给UI线程执行
    ///
    /// # Parameters
    ///
    /// - `param` 参数
    /// - `handler` 接收`param`参数的回调过程并在UI线程中执行
    ///
    /// # Returns
    ///
    /// 成功接收请求后返回`true`否则返回`false`
    pub async fn dispatch_with_param<P, H>(&self, param: P, handler: H) -> bool
    where
        P: Send + 'static,
        H: FnOnce(&mut T, P) + Send + 'static
    {
        if self.alive.is_dead() {
            return false;
        }
        let handler = unsafe {
            let this = self.this.clone();
            Box::new(move |param: UnsafeBox<()>, alive: AliveState| {
                let param = param.cast::<P>().unpack();
                if alive.is_alive() {
                    let this = &mut *this.into_raw();
                    handler(this, param);
                }
            })
        };
        let param = unsafe { UnsafeBox::pack(param).cast::<()>() };
        self.dsp.dispatch_invoke(param, handler, self.alive.clone()).await
    }

    /// 派发执行异常信息给UI线程
    async fn dispatch_panic(&self, panic_info: &str) -> bool {
        self.dsp
            .dispatch_panic(format!("{}\r\nbacktrace:\r\n{:?}", panic_info, backtrace::Backtrace::new()))
            .await
    }
}

impl<T> Clone for HandlerDispatcher<T> {
    fn clone(&self) -> Self {
        HandlerDispatcher {
            dsp: self.dsp.clone(),
            this: unsafe { self.this.clone() },
            alive: self.alive.clone()
        }
    }
}