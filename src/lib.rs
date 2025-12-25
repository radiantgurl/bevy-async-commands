#[cfg(debug_assertions)]
use std::sync::{Arc, atomic::AtomicBool};

use std::{future::Future, mem::transmute, ops::{Deref, DerefMut}, marker::PhantomData};

use bevy_app::{App, Plugin};
use bevy_async_ecs::{AsyncEcsPlugin, AsyncWorld};
use bevy_ecs::{resource::Resource, system::Commands, world::{FromWorld, World}};
use bevy_tasks::{AsyncComputeTaskPool, Task, futures::check_ready, tick_global_task_pools_on_main_thread};
use event_listener_strategy::event_listener::Event;

#[derive(Clone, Copy)]
pub struct BevyAsyncCommandsPlugin;

#[derive(Resource, Clone)]
pub struct BevyAsyncWorld(pub AsyncWorld);

#[cfg(not(debug_assertions))]
pub struct AsyncCommands<'w: 's + 'aw, 's, 'aw>(Option<Commands<'w, 's>>, Option<Event>, PhantomData<&'aw mut AsyncWorld>);
#[cfg(debug_assertions)]
pub struct AsyncCommands<'w: 's + 'aw, 's, 'aw>(Option<Commands<'w, 's>>, Option<Event>, Arc<AtomicBool>, PhantomData<&'aw mut AsyncWorld>);

impl Plugin for BevyAsyncCommandsPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(AsyncEcsPlugin);
        
        let async_world = AsyncWorld::from_world(app.world_mut());
        app.insert_resource(BevyAsyncWorld(async_world));
    }
}

impl<'w: 's, 's, 'aw> DerefMut for AsyncCommands<'w, 's, 'aw> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}

impl<'w: 's, 's, 'aw> Deref for AsyncCommands<'w, 's, 'aw> {
    type Target = Commands<'w, 's>;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

#[cfg(not(debug_assertions))]
impl<'w, 's, 'aw> Drop for AsyncCommands<'w, 's, 'aw> {
    fn drop(&mut self) {
        drop(self.0.take());
        if let Some(ev) = self.1.take() {
            ev.notify_additional(1);
        }
    }
}
#[cfg(debug_assertions)]
impl<'w, 's, 'aw> Drop for AsyncCommands<'w, 's, 'aw> {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;

        drop(self.0.take());
        self.2.store(true, Relaxed);
        if let Some(ev) = self.1.take() {
            ev.notify_additional(1);
        }
    }
}

trait AsyncWorldCommandExtInternal: Send + Sync {
    fn commands_internal<'w: 's + 'aw, 's, 'aw>(&mut self) -> impl Future<Output = AsyncCommands<'w, 's, 'aw>> + Send;
}

pub trait AsyncWorldCommandExt: Send + Sync {
    fn commands<'w: 's + 'aw, 's, 'aw>(&'aw mut self) -> impl Future<Output = AsyncCommands<'w, 's, 'aw>> + Send;
}

#[cfg(not(debug_assertions))]
impl AsyncWorldCommandExtInternal for AsyncWorld {
    async fn commands_internal<'w: 's, 's, 'aw>(&mut self) -> AsyncCommands<'w, 's, 'aw> {
        let (s,r) = async_channel::bounded(1);
        let ev = Event::new();
        let mut listener = ev.listen();

        self.apply(move |w: &mut World| {
            let c = w.commands();

            // SAFETY: We are awaiting on the event listener to drop the reference.
            unsafe {
                s.try_send(transmute::<_, Commands<'static, 'static>>(c)).unwrap();
            }

            while check_ready(&mut listener).is_none() {
                tick_global_task_pools_on_main_thread();
            }
            // By this point, the AsyncCommands that had access to the commands has been deleted.

        }).await;
        let commands: Commands<'w, 's> = r.recv().await.unwrap();
        AsyncCommands::<'_, '_, '_>(Some(commands), Some(ev), PhantomData::default())
    }
}

#[cfg(debug_assertions)]
impl AsyncWorldCommandExtInternal for AsyncWorld {
    async fn commands_internal<'w: 's + 'aw, 's, 'aw>(&mut self) -> AsyncCommands<'w, 's, 'aw> {
        use std::sync::atomic::Ordering::Relaxed;

        let (s,r) = async_channel::bounded(1);

        let ev = Event::new();
        let mut listener = ev.listen();

        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();

        self.apply(move |w: &mut World| {
            let c = w.commands();

            // SAFETY: We are awaiting on the event listener to drop the reference.
            unsafe {
                s.try_send(transmute::<_, Commands<'static, 'static>>(c)).unwrap();
            }

            while check_ready(&mut listener).is_none() {
                tick_global_task_pools_on_main_thread();
            }
            // By this point, the AsyncCommands that had access to the commands has been deleted.

            assert!(flag_clone.load(Relaxed), "reference not dropped after signalling");

        }).await;
        let commands: Commands<'w, 's> = r.recv().await.unwrap();
        AsyncCommands::<'_, '_, '_>(Some(commands), Some(ev), flag, PhantomData::default())
    }
}

impl AsyncWorldCommandExt for AsyncWorld {
    fn commands<'w: 's + 'aw, 's, 'aw>(&'aw mut self) -> impl Future<Output = AsyncCommands<'w, 's, 'aw>> + Send {
        self.commands_internal()
    }
}

pub trait AsyncCommandsExt<'w, 's>: Send + Sync {
    fn async_world(&mut self) -> Task<AsyncWorld>;
    // fn async_commands<'ns, 'aw>(&mut self) -> AsyncCommands<'w, 'ns, 'aw> where 's: 'ns, 'w: 'aw;
    fn async_commands(self) -> AsyncCommands<'w, 's, 's>;
}

impl<'w, 's> AsyncCommandsExt<'w, 's> for Commands<'w, 's> {
    fn async_world(&mut self) -> Task<AsyncWorld> {
        let (s, r) = async_channel::bounded(1);
        self.queue(move |w: &mut World| {
            s.try_send(w.resource::<BevyAsyncWorld>().0.clone()).unwrap();
        });
        AsyncComputeTaskPool::get().spawn(async move {
            r.recv().await.unwrap()
        })
    }
    // #[cfg(not(debug_assertions))]
    // fn async_commands(&mut self) -> AsyncCommands<'_, '_, '_> {
    //     AsyncCommands(Some(self.reborrow()), None, PhantomData::default())
    // }
    // #[cfg(debug_assertions)]
    // fn async_commands<'ns, 'aw>(&mut self) -> AsyncCommands<'w, 'ns, 'aw> where 's: 'ns, 'w: 'aw {
    //     AsyncCommands(Some(self.reborrow()), None, Arc::new(AtomicBool::new(false)), PhantomData::default())
    // }
    #[cfg(not(debug_assertions))]
    fn async_commands(self) -> AsyncCommands<'w, 's, 's> {
        AsyncCommands(Some(self.reborrow()), None, PhantomData::default())
    }
    #[cfg(debug_assertions)]
    fn async_commands(self) -> AsyncCommands<'w, 's, 's> {
        AsyncCommands(Some(self), None, Arc::default(), PhantomData::default())
    }
}
 
#[cfg(test)]
mod tests {
    use bevy::MinimalPlugins;
    use bevy_app::App;
    use bevy_async_ecs::AsyncWorld;
    use bevy_ecs::{system::Commands, world::FromWorld};
    use bevy_tasks::AsyncComputeTaskPool;
    use crate::{AsyncWorldCommandExt, BevyAsyncCommandsPlugin, AsyncCommandsExt};

    fn test_system() {
        println!("rocking");
    }

    fn test_async_system(c: Commands) {
        let mut async_commands = c.async_commands();
        AsyncComputeTaskPool::get().spawn(async move {
            async_commands.run_system_cached(test_system);
        }).detach();
    }

    #[test]
    fn generic() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, BevyAsyncCommandsPlugin));
        let mut async_world = AsyncWorld::from_world(app.world_mut());
        let (s,r) = async_channel::bounded(1);
        AsyncComputeTaskPool::get().spawn(async move {
            let mut commands = async_world.commands().await;
            commands.run_system_cached(test_system);
            s.send(()).await.unwrap();
        }).detach();
        while r.try_recv().is_err() {
            app.update();
        }
    }
}