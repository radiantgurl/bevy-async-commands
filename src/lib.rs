#[cfg(debug_assertions)]
use std::sync::{Arc, atomic::AtomicBool};

use std::{future::Future, mem::transmute, ops::{Deref, DerefMut}};

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
pub struct AsyncCommands<'w: 's, 's>(Option<Commands<'w, 's>>, Option<Event>);
#[cfg(debug_assertions)]
pub struct AsyncCommands<'w: 's, 's>(Option<Commands<'w, 's>>, Option<Event>, Arc<AtomicBool>);

impl Plugin for BevyAsyncCommandsPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(AsyncEcsPlugin);
        
        let async_world = AsyncWorld::from_world(app.world_mut());
        app.insert_resource(BevyAsyncWorld(async_world));
    }
}

impl<'w: 's, 's> DerefMut for AsyncCommands<'w, 's> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}

impl<'w: 's, 's> Deref for AsyncCommands<'w, 's> {
    type Target = Commands<'w, 's>;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

#[cfg(not(debug_assertions))]
impl<'w, 's> Drop for AsyncCommands<'w, 's> {
    fn drop(&mut self) {
        drop(self.0.take());
        if let Some(ev) = self.1.take() {
            ev.notify_additional(1);
        }
    }
}
#[cfg(debug_assertions)]
impl<'w, 's> Drop for AsyncCommands<'w, 's> {
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
    fn commands_internal<'w: 's, 's>(&mut self) -> impl Future<Output = AsyncCommands<'w, 's>> + Send;
}

pub trait AsyncWorldCommandExt: Send + Sync {
    fn commands<'w: 's, 's>(&mut self) -> impl Future<Output = AsyncCommands<'w, 's>> + Send;
}

#[cfg(not(debug_assertions))]
impl AsyncWorldCommandExtInternal for AsyncWorld {
    async fn commands_internal<'w: 's, 's>(&mut self) -> AsyncCommands<'w, 's> {
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
        AsyncCommands::<'_, '_>(Some(commands), Some(ev))
    }
}

#[cfg(debug_assertions)]
impl AsyncWorldCommandExtInternal for AsyncWorld {
    async fn commands_internal<'w: 's, 's>(&mut self) -> AsyncCommands<'w, 's> {
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
        AsyncCommands::<'_, '_>(Some(commands), Some(ev), flag)
    }
}

impl AsyncWorldCommandExt for AsyncWorld {
    fn commands<'w: 's, 's>(&mut self) -> impl Future<Output = AsyncCommands<'w, 's>> + Send {
        self.commands_internal()
    }
}

pub trait AsyncCommandsExt: Send + Sync {
    fn async_world(&mut self) -> Task<AsyncWorld>;
    fn async_commands<'s>(self) -> impl Future<Output=AsyncCommands<'static, 's>> + Send;
}

impl<'w, 's> AsyncCommandsExt for Commands<'w, 's> {
    fn async_world(&mut self) -> Task<AsyncWorld> {
        let (s, r) = async_channel::bounded(1);
        self.queue(move |w: &mut World| {
            s.try_send(w.resource::<BevyAsyncWorld>().0.clone()).unwrap();
        });
        AsyncComputeTaskPool::get().spawn(async move {
            r.recv().await.unwrap()
        })
    }
    fn async_commands<'s_1>(mut self) -> impl Future<Output=AsyncCommands<'static, 's_1>> + Send {
        let task = self.async_world();
        AsyncComputeTaskPool::get().spawn(async move {
            let mut w = task.await;
            w.commands().await
        })
    }
}
 
#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use bevy::MinimalPlugins;
    use bevy_app::App;
    use bevy_async_ecs::AsyncWorld;
    use bevy_ecs::{component::Component, system::Commands, world::FromWorld};
    use bevy_tasks::AsyncComputeTaskPool;
    use crate::{AsyncWorldCommandExt, BevyAsyncCommandsPlugin, AsyncCommandsExt};

    #[derive(Component, Clone, Copy)]
    pub struct EntityPush(i32);

    fn test_system() {
        println!("rocking");
    }

    fn test_async_system(mut c: Commands) {
        let async_world = c.async_world();
        AsyncComputeTaskPool::get().spawn(async move {
            let mut world = async_world.await;
            let mut c= world.commands().await;
            c.spawn((EntityPush(42),));
            println!("command pushed");
        }).detach();
    }

    #[test]
    fn generic() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, BevyAsyncCommandsPlugin));
        let mut async_world = AsyncWorld::from_world(app.world_mut());
        AsyncComputeTaskPool::get().spawn(async move {
            let mut commands = async_world.commands().await;
            commands.run_system_cached(test_system);
            commands.run_system_cached(test_async_system);
        }).detach();
        let d = Instant::now().checked_add(Duration::from_secs(1)).unwrap();
        while Instant::now() < d {
            app.update();
        }
        let mut data = app.world_mut().query::<&EntityPush>();
        let r = data.single(app.world()).unwrap();
        println!("{}",r.0);
    }
}