use crate::{
    current_platform, AnyWindowHandle, Context, LayoutId, MainThreadOnly, Platform, Reference,
    RootView, TextSystem, Window, WindowContext, WindowHandle, WindowId,
};
use anyhow::{anyhow, Result};
use collections::{HashMap, VecDeque};
use futures::{future, Future};
use parking_lot::Mutex;
use slotmap::SlotMap;
use smallvec::SmallVec;
use std::{
    any::Any,
    marker::PhantomData,
    sync::{Arc, Weak},
};

#[derive(Clone)]
pub struct App(Arc<Mutex<AppContext>>);

impl App {
    pub fn production() -> Self {
        Self::new(current_platform())
    }

    #[cfg(any(test, feature = "test"))]
    pub fn test() -> Self {
        Self::new(Arc::new(super::TestPlatform::new()))
    }

    fn new(platform: Arc<dyn Platform>) -> Self {
        let dispatcher = platform.dispatcher();
        let text_system = Arc::new(TextSystem::new(platform.text_system()));
        let mut entities = SlotMap::with_key();
        let unit_entity_id = entities.insert(Some(Box::new(()) as Box<dyn Any + Send>));
        Self(Arc::new_cyclic(|this| {
            Mutex::new(AppContext {
                this: this.clone(),
                platform: MainThreadOnly::new(platform, dispatcher),
                text_system,
                unit_entity_id,
                entities,
                windows: SlotMap::with_key(),
                pending_updates: 0,
                pending_effects: Default::default(),
                observers: Default::default(),
                layout_id_buffer: Default::default(),
            })
        }))
    }

    pub fn run<F>(self, on_finish_launching: F)
    where
        F: 'static + FnOnce(&mut AppContext),
    {
        let this = self.clone();
        let platform = self.0.lock().platform.clone();
        platform.borrow_on_main_thread().run(Box::new(move || {
            let cx = &mut *this.0.lock();
            on_finish_launching(cx);
        }));
    }
}

type Handlers = SmallVec<[Arc<dyn Fn(&mut AppContext) -> bool + Send + Sync + 'static>; 2]>;

pub struct AppContext {
    this: Weak<Mutex<AppContext>>,
    platform: MainThreadOnly<dyn Platform>,
    text_system: Arc<TextSystem>,
    pub(crate) unit_entity_id: EntityId,
    pub(crate) entities: SlotMap<EntityId, Option<Box<dyn Any + Send>>>,
    pub(crate) windows: SlotMap<WindowId, Option<Window>>,
    pending_updates: usize,
    pub(crate) pending_effects: VecDeque<Effect>,
    pub(crate) observers: HashMap<EntityId, Handlers>,
    // We recycle this memory across layout requests.
    pub(crate) layout_id_buffer: Vec<LayoutId>,
}

impl AppContext {
    pub fn text_system(&self) -> &Arc<TextSystem> {
        &self.text_system
    }

    pub fn spawn_on_main<F, R>(
        &mut self,
        f: impl FnOnce(&dyn Platform, &mut Self) -> F + Send + 'static,
    ) -> impl Future<Output = R>
    where
        F: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        let this = self.this.upgrade().unwrap();
        self.platform.read(move |platform| {
            let cx = &mut *this.lock();
            f(platform, cx)
        })
    }

    pub fn open_window<S: 'static + Send>(
        &mut self,
        options: crate::WindowOptions,
        build_root_view: impl FnOnce(&mut WindowContext) -> RootView<S> + Send + 'static,
    ) -> impl Future<Output = WindowHandle<S>> {
        let id = self.windows.insert(None);
        let handle = WindowHandle::new(id);
        self.spawn_on_main(move |platform, cx| {
            let mut window = Window::new(handle.into(), options, platform);
            let root_view = build_root_view(&mut WindowContext::mutable(cx, &mut window));
            window.root_view.replace(Box::new(root_view));
            cx.windows.get_mut(id).unwrap().replace(window);
            future::ready(handle)
        })
    }

    pub(crate) fn update_window<R>(
        &mut self,
        handle: AnyWindowHandle,
        update: impl FnOnce(&mut WindowContext) -> R,
    ) -> Result<R> {
        let mut window = self
            .windows
            .get_mut(handle.id)
            .ok_or_else(|| anyhow!("window not found"))?
            .take()
            .unwrap();

        let result = update(&mut WindowContext::mutable(self, &mut window));
        window.dirty = true;

        self.windows
            .get_mut(handle.id)
            .ok_or_else(|| anyhow!("window not found"))?
            .replace(window);

        Ok(result)
    }

    fn update<R>(&mut self, update: impl FnOnce(&mut Self) -> R) -> R {
        self.pending_updates += 1;
        let result = update(self);
        self.pending_updates -= 1;
        if self.pending_updates == 0 {
            self.flush_effects();
        }
        result
    }

    fn flush_effects(&mut self) {
        while let Some(effect) = self.pending_effects.pop_front() {
            match effect {
                Effect::Notify(entity_id) => self.apply_notify_effect(entity_id),
            }
        }
    }

    fn apply_notify_effect(&mut self, updated_entity: EntityId) {
        if let Some(mut handlers) = self.observers.remove(&updated_entity) {
            handlers.retain(|handler| handler(self));
            if let Some(new_handlers) = self.observers.remove(&updated_entity) {
                handlers.extend(new_handlers);
            }
            self.observers.insert(updated_entity, handlers);
        }
    }
}

impl Context for AppContext {
    type EntityContext<'a, 'w, T: Send + Sync + 'static> = ModelContext<'a, T>;

    fn entity<T: Send + Sync + 'static>(
        &mut self,
        build_entity: impl FnOnce(&mut Self::EntityContext<'_, '_, T>) -> T,
    ) -> Handle<T> {
        let id = self.entities.insert(None);
        let entity = Box::new(build_entity(&mut ModelContext::mutable(self, id)));
        self.entities.get_mut(id).unwrap().replace(entity);

        Handle::new(id)
    }

    fn update_entity<T: Send + Sync + 'static, R>(
        &mut self,
        handle: &Handle<T>,
        update: impl FnOnce(&mut T, &mut Self::EntityContext<'_, '_, T>) -> R,
    ) -> R {
        let mut entity = self
            .entities
            .get_mut(handle.id)
            .unwrap()
            .take()
            .unwrap()
            .downcast::<T>()
            .unwrap();

        let result = update(&mut *entity, &mut ModelContext::mutable(self, handle.id));
        self.entities.get_mut(handle.id).unwrap().replace(entity);
        result
    }
}

pub struct ModelContext<'a, T> {
    app: Reference<'a, AppContext>,
    entity_type: PhantomData<T>,
    entity_id: EntityId,
}

impl<'a, T: Send + Sync + 'static> ModelContext<'a, T> {
    pub(crate) fn mutable(app: &'a mut AppContext, entity_id: EntityId) -> Self {
        Self {
            app: Reference::Mutable(app),
            entity_type: PhantomData,
            entity_id,
        }
    }

    fn immutable(app: &'a AppContext, entity_id: EntityId) -> Self {
        Self {
            app: Reference::Immutable(app),
            entity_type: PhantomData,
            entity_id,
        }
    }

    fn update<R>(&mut self, update: impl FnOnce(&mut T, &mut Self) -> R) -> R {
        let mut entity = self
            .app
            .entities
            .get_mut(self.entity_id)
            .unwrap()
            .take()
            .unwrap();
        let result = update(entity.downcast_mut::<T>().unwrap(), self);
        self.app
            .entities
            .get_mut(self.entity_id)
            .unwrap()
            .replace(entity);
        result
    }

    pub fn handle(&self) -> WeakHandle<T> {
        WeakHandle {
            id: self.entity_id,
            entity_type: PhantomData,
        }
    }

    pub fn observe<E: Send + Sync + 'static>(
        &mut self,
        handle: &Handle<E>,
        on_notify: impl Fn(&mut T, Handle<E>, &mut ModelContext<'_, T>) + Send + Sync + 'static,
    ) {
        let this = self.handle();
        let handle = handle.downgrade();
        self.app
            .observers
            .entry(handle.id)
            .or_default()
            .push(Arc::new(move |cx| {
                if let Some((this, handle)) = this.upgrade(cx).zip(handle.upgrade(cx)) {
                    this.update(cx, |this, cx| on_notify(this, handle, cx));
                    true
                } else {
                    false
                }
            }));
    }

    pub fn notify(&mut self) {
        self.app
            .pending_effects
            .push_back(Effect::Notify(self.entity_id));
    }
}

impl<'a, T: 'static> Context for ModelContext<'a, T> {
    type EntityContext<'b, 'c, U: Send + Sync + 'static> = ModelContext<'b, U>;

    fn entity<U: Send + Sync + 'static>(
        &mut self,
        build_entity: impl FnOnce(&mut Self::EntityContext<'_, '_, U>) -> U,
    ) -> Handle<U> {
        self.app.entity(build_entity)
    }

    fn update_entity<U: Send + Sync + 'static, R>(
        &mut self,
        handle: &Handle<U>,
        update: impl FnOnce(&mut U, &mut Self::EntityContext<'_, '_, U>) -> R,
    ) -> R {
        self.app.update_entity(handle, update)
    }
}

slotmap::new_key_type! { pub struct EntityId; }

pub struct Handle<T> {
    pub(crate) id: EntityId,
    pub(crate) entity_type: PhantomData<T>,
}

impl<T: Send + Sync + 'static> Handle<T> {
    fn new(id: EntityId) -> Self {
        Self {
            id,
            entity_type: PhantomData,
        }
    }

    pub fn downgrade(&self) -> WeakHandle<T> {
        WeakHandle {
            id: self.id,
            entity_type: self.entity_type,
        }
    }

    /// Update the entity referenced by this handle with the given function.
    ///
    /// The update function receives a context appropriate for its environment.
    /// When updating in an `AppContext`, it receives a `ModelContext`.
    /// When updating an a `WindowContext`, it receives a `ViewContext`.
    pub fn update<C: Context, R>(
        &self,
        cx: &mut C,
        update: impl FnOnce(&mut T, &mut C::EntityContext<'_, '_, T>) -> R,
    ) -> R {
        cx.update_entity(self, update)
    }
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            entity_type: PhantomData,
        }
    }
}

pub struct WeakHandle<T> {
    pub(crate) id: EntityId,
    pub(crate) entity_type: PhantomData<T>,
}

impl<T: Send + Sync + 'static> WeakHandle<T> {
    pub fn upgrade(&self, cx: &impl Context) -> Option<Handle<T>> {
        // todo!("Actually upgrade")
        Some(Handle {
            id: self.id,
            entity_type: self.entity_type,
        })
    }

    /// Update the entity referenced by this handle with the given function if
    /// the referenced entity still exists. Returns an error if the entity has
    /// been released.
    ///
    /// The update function receives a context appropriate for its environment.
    /// When updating in an `AppContext`, it receives a `ModelContext`.
    /// When updating an a `WindowContext`, it receives a `ViewContext`.
    pub fn update<C: Context, R>(
        &self,
        cx: &mut C,
        update: impl FnOnce(&mut T, &mut C::EntityContext<'_, '_, T>) -> R,
    ) -> Result<R> {
        if let Some(this) = self.upgrade(cx) {
            Ok(cx.update_entity(&this, update))
        } else {
            Err(anyhow!("entity released"))
        }
    }
}

pub(crate) enum Effect {
    Notify(EntityId),
}

#[cfg(test)]
mod tests {
    use super::AppContext;

    #[test]
    fn test_app_context_send_sync() {
        // This will not compile if `AppContext` does not implement `Send`
        fn assert_send<T: Send>() {}
        assert_send::<AppContext>();
    }
}
