//! An API for plugging in adapters.

use adapter::{ Adapter, AdapterWatchGuard, WatchEvent as AdapterWatchEvent };
use transact::InsertInMap;

use foxbox_taxonomy::api::{ API, Error, InternalError, WatchEvent, ResultMap };
use foxbox_taxonomy::selector::*;
use foxbox_taxonomy::services::*;
use foxbox_taxonomy::util::*;
use foxbox_taxonomy::values::*;

use transformable_channels::mpsc::*;

use std::cell::RefCell;
use std::collections::{ HashMap, HashSet };
use std::collections::hash_map::Entry;
use std::hash::{ Hash, Hasher };
use std::ops::Deref;
use std::rc::Rc;
use std::sync::{ Arc, Mutex };
use std::sync::atomic::{ AtomicBool, Ordering };

/// Data and metadata on an adapter.
struct AdapterData {
    /// The implementation of the adapter.
    adapter: Box<Adapter>,

    /// The services for this adapter.
    services: HashMap<Id<ServiceId>, Rc<RefCell<Service>>>,
}

impl AdapterData {
    fn new(adapter: Box<Adapter>) -> Self {
        AdapterData {
            adapter: adapter,
            services: HashMap::new(),
        }
    }
}
impl Deref for AdapterData {
    type Target = Box<Adapter>;
    fn deref(&self) -> &Self::Target {
        &self.adapter
    }
}

trait Tagged {
    fn insert_tags(&mut self, tags: &[Id<TagId>]);
    fn remove_tags(&mut self, tags: &[Id<TagId>]);
}

impl<T> Tagged for Channel<T> where T: IOMechanism {
    fn insert_tags(&mut self, tags: &[Id<TagId>]) {
        for tag in tags {
            let _ = self.tags.insert(tag.clone());
        }
    }
    fn remove_tags(&mut self, tags: &[Id<TagId>]) {
        for tag in tags {
            let _ = self.tags.remove(tag);
        }
    }
}

impl Hash for WatcherData {
     fn hash<H>(&self, state: &mut H) where H: Hasher {
         self.key.hash(state)
     }
}
impl Eq for WatcherData {}
impl PartialEq for WatcherData {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
struct GetterData {
    getter: Channel<Getter>,
    watchers: HashSet<Arc<WatcherData>>,
}
impl GetterData {
    fn new(getter: Channel<Getter>) -> Self {
        GetterData {
            getter: getter,
            watchers: HashSet::new(),
        }
    }
}
impl SelectedBy<GetterSelector> for GetterData {
    fn matches(&self, selector: &GetterSelector) -> bool {
        self.getter.matches(selector)
    }
}
impl Deref for GetterData {
    type Target = Channel<Getter>;
    fn deref(&self) -> &Self::Target {
        &self.getter
    }
}
impl Tagged for GetterData {
    fn insert_tags(&mut self, tags: &[Id<TagId>]) {
        self.getter.insert_tags(tags)
    }
    fn remove_tags(&mut self, tags: &[Id<TagId>]) {
        self.getter.remove_tags(tags)
    }
}
struct SetterData {
    setter: Channel<Setter>,
}
impl SetterData {
    fn new(setter: Channel<Setter>) -> Self {
        SetterData {
            setter: setter
        }
    }
}
impl SelectedBy<SetterSelector> for SetterData {
    fn matches(&self, selector: &SetterSelector) -> bool {
        self.setter.matches(selector)
    }
}
impl Tagged for SetterData {
    fn insert_tags(&mut self, tags: &[Id<TagId>]) {
        self.setter.insert_tags(tags)
    }
    fn remove_tags(&mut self, tags: &[Id<TagId>]) {
        self.setter.remove_tags(tags)
    }
}
impl Deref for SetterData {
    type Target = Channel<Setter>;
    fn deref(&self) -> &Self::Target {
        &self.setter
    }
}


struct WatcherData {
    watch: Vec<(Vec<GetterSelector>, Exactly<Range>)>,
    on_event: Box<ExtSender<WatchEvent>>,
    key: usize,
    guards: RefCell<Vec<Box<AdapterWatchGuard>>>,
    getters: RefCell<HashSet<Id<Getter>>>,
    is_dropped: Arc<AtomicBool>,
}

impl WatcherData {
    fn new(key: usize, watch: Vec<(Vec<GetterSelector>, Exactly<Range>)>, is_dropped: &Arc<AtomicBool>, on_event: Box<ExtSender<WatchEvent>>) -> Self {
        WatcherData {
            key: key,
            on_event: on_event,
            watch: watch,
            is_dropped: is_dropped.clone(),
            guards: RefCell::new(Vec::new()),
            getters: RefCell::new(HashSet::new()),
        }
    }

    fn push_guard(&self, guard: Box<AdapterWatchGuard>) {
        self.guards.borrow_mut().push(guard);
    }

    fn push_getter(&self, id: &Id<Getter>) {
        self.getters.borrow_mut().insert(id.clone());
    }
}

pub struct WatchMap {
    /// A counter of all watchers that have been added to the system.
    /// Used to generate unique keys.
    counter: usize,
    watchers: HashMap<usize, Arc<WatcherData>>,
}
impl WatchMap {
    fn new() -> Self {
        WatchMap {
            counter: 0,
            watchers: HashMap::new()
        }
    }
    fn create(&mut self, watch: Vec<(Vec<GetterSelector>, Exactly<Range>)>, is_dropped: &Arc<AtomicBool>, on_event: Box<ExtSender<WatchEvent>>) -> Arc<WatcherData> {
        let id = self.counter;
        self.counter += 1;
        let watcher = Arc::new(WatcherData::new(id, watch, is_dropped, on_event));
        self.watchers.insert(id, watcher.clone());
        watcher
    }
    fn remove(&mut self, key: usize) -> Option<Arc<WatcherData>> {
        self.watchers.remove(&key)
    }
}

impl Default for WatchMap {
    fn default() -> Self {
        Self::new()
    }
}

/// A data structure that causes cancellation of a watch when dropped.
pub struct WatchGuard {
    /// The channel used to request unregistration.
    tx_owner: Box<ExtSender<Op>>,

    /// The cancellation key.
    key: usize,

    /// A channel controlling the dedicated watcher thread.
    tx_watcher: Box<ExtSender<WatchEvent>>,

    /// Once dropped, the watch callbacks will stopped being called. Note
    /// that dropping this value is not sufficient to cancel the watch, as
    /// the adapters will continue sending updates.
    is_dropped: Arc<AtomicBool>,
}
impl WatchGuard {
    pub fn new(tx_owner: Box<ExtSender<Op>>, tx_watcher: Box<ExtSender<WatchEvent>>, key: usize, is_dropped: Arc<AtomicBool>) -> Self {
        WatchGuard {
            tx_owner: tx_owner,
            tx_watcher: tx_watcher,
            key: key,
            is_dropped: is_dropped
        }
    }
}
impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.is_dropped.store(true, Ordering::Relaxed);
        // Ignore result. If the thread is already down, just forget about it.
        let _ = self.tx_owner.send(Op::UnregisterChannelWatch {
            key: self.key
        });
    }
}

pub struct AdapterManagerState {
    /// Adapters, indexed by their id.
    adapter_by_id: HashMap<Id<AdapterId>, AdapterData>,

    /// Services, indexed by their id.
    service_by_id: HashMap<Id<ServiceId>, Rc<RefCell<Service>>>,

    /// Getters, indexed by their id. // FIXME: We have two copies of each setter, the other one in Service!
    getter_by_id: HashMap<Id<Getter>, GetterData>,

    /// Setters, indexed by their id. // FIXME: We have two copies of each setter, the other one in Service!
    setter_by_id: HashMap<Id<Setter>, SetterData>,

    /// The set of watchers registered. Used both when we add/remove channels
    /// and a when a new value is available from a getter channel.
    watchers: Arc<Mutex<WatchMap>>,
}

impl AdapterManagerState {
    /// Auxiliary function to remove a service, once the mutex has been acquired.
    /// Clients should rather use AdapterManager::remove_service.
    fn aux_remove_service(&mut self, id: &Id<ServiceId>) -> Result<Id<AdapterId>, Error> {
        let (adapter, service) = match self.service_by_id.remove(&id) {
            None => return Err(Error::InternalError(InternalError::NoSuchService(id.clone()))),
            Some(service) => {
                let adapter = service.borrow().adapter.clone();
                (adapter, service)
            }
        };
        for id in service.borrow().getters.keys() {
            let _ignored = self.getter_by_id.remove(id);
        }
        for id in service.borrow().setters.keys() {
            let _ignored = self.setter_by_id.remove(id);
        }
        Ok(adapter)
    }

    fn with_services<F>(&self, selectors: Vec<ServiceSelector>, mut cb: F) where F: FnMut(&Rc<RefCell<Service>>) {
        for service in self.service_by_id.values() {
            let matches = selectors.iter().find(|selector| {
                selector.matches(&*service.borrow())
            }).is_some();
            if matches {
                cb(service);
            }
        };
    }

    /// Iterate over all channels that match any selector in a slice.
    fn with_channels<S, K, V, F>(selectors: Vec<S>, map: &HashMap<Id<K>, V>, mut cb: F)
        where F: FnMut(&V),
              V: SelectedBy<S>,
    {
        for (_, data) in map.iter() {
            let matches = selectors.iter().find(|selector| {
                data.matches(selector)
            }).is_some();
            if matches {
                cb(data);
            }
        }
    }

    /// Iterate mutably over all channels that match any selector in a slice.
    fn with_channels_mut<S, K, V, F>(selectors: Vec<S>, map: &mut HashMap<Id<K>, V>, mut cb: F)
        where F: FnMut(&mut V),
              V: SelectedBy<S>,
    {
        for (_, data) in map.iter_mut() {
            let matches = selectors.iter().find(|selector| {
                data.matches(selector)
            }).is_some();
            if matches {
                cb(data);
            }
        }
    }

    /// Iterate over all channels that match any selector in a slice.
    fn aux_get_channels<S, K, V, T>(selectors: Vec<S>, map: &HashMap<Id<K>, V>) -> Vec<Channel<T>>
        where V: SelectedBy<S> + Deref<Target = Channel<T>>,
              T: IOMechanism,
              Channel<T>: Clone
    {
        let mut result = Vec::new();
        Self::with_channels(selectors, map, |data| {
            result.push((*data.deref()).clone());
        });
        result
    }

    fn aux_add_channel_tags<S, K, V>(selectors: Vec<S>, tags: Vec<Id<TagId>>, map: &mut HashMap<Id<K>, V>) -> usize
        where V: SelectedBy<S> + Tagged
    {
        let mut result = 0;
        Self::with_channels_mut(selectors, map, |mut data| {
            data.insert_tags(&tags);
            result += 1;
        });
        result
    }

    fn aux_remove_channel_tags<S, K, V>(selectors: Vec<S>, tags: Vec<Id<TagId>>, map: &mut HashMap<Id<K>, V>) -> usize
        where V: SelectedBy<S> + Tagged
    {
        let mut result = 0;
        Self::with_channels_mut(selectors, map, |mut data| {
            data.remove_tags(&tags);
            result += 1;
        });
        result
    }

    /*
        fn iter_channels<'a, S, K, V>(selectors: Vec<S>, map: &HashMap<Id<K>, V>) ->
            Filter<Values<'a, Id<K>, V>, &'a (Fn(&'a V) -> bool)>
            where V: SelectedBy<S>
        {
            let cb : &'a Fn(&'a V) -> bool + 'a = |data: &'a V| {
                selectors.iter().find(|selector| {
                    data.matches(selector)
                }).is_some()
            };
            map.values()
                .filter(cb)
        }
    */

}

pub enum Op {
    AddAdapter {
        adapter: Box<Adapter>,
        tx: RawSender<Result<(), Error>>,
    },
    RemoveAdapter {
        id: Id<AdapterId>,
        tx: RawSender<Result<(), Error>>,
    },
    AddService {
        service: Service,
        tx: RawSender<Result<(), Error>>,
    },
    RemoveService {
        id: Id<ServiceId>,
        tx: RawSender<Result<(), Error>>,
    },
    AddGetter {
        getter: Channel<Getter>,
        tx: RawSender<Result<(), Error>>,
    },
    RemoveGetter {
        id: Id<Getter>,
        tx: RawSender<Result<(), Error>>,
    },
    AddSetter {
        setter: Channel<Setter>,
        tx: RawSender<Result<(), Error>>,
    },
    RemoveSetter {
        id: Id<Setter>,
        tx: RawSender<Result<(), Error>>,
    },
    GetServices {
        selectors: Vec<ServiceSelector>,
        tx: RawSender<Vec<Service>>,
    },
    AddServiceTags {
        selectors: Vec<ServiceSelector>,
        tags: Vec<Id<TagId>>,
        tx: RawSender<usize>,
    },
    RemoveServiceTags {
        selectors: Vec<ServiceSelector>,
        tags: Vec<Id<TagId>>,
        tx: RawSender<usize>,
    },
    GetGetterChannels {
        selectors: Vec<GetterSelector>,
        tx: RawSender<Vec<Channel<Getter>>>,
    },
    GetSetterChannels {
        selectors: Vec<SetterSelector>,
        tx: RawSender<Vec<Channel<Setter>>>,
    },
    AddGetterTags {
        selectors: Vec<GetterSelector>,
        tags: Vec<Id<TagId>>,
        tx: RawSender<usize>,
    },
    AddSetterTags {
        selectors: Vec<SetterSelector>,
        tags: Vec<Id<TagId>>,
        tx: RawSender<usize>,
    },
    RemoveGetterTags {
        selectors: Vec<GetterSelector>,
        tags: Vec<Id<TagId>>,
        tx: RawSender<usize>,
    },
    RemoveSetterTags {
        selectors: Vec<SetterSelector>,
        tags: Vec<Id<TagId>>,
        tx: RawSender<usize>,
    },
    FetchValues {
        selectors: Vec<GetterSelector>,
        tx: RawSender<ResultSet<Id<Getter>, Option<Value>, Error>>,
    },
    SendValues {
        keyvalues: Vec<(Vec<SetterSelector>, Value)>,
        tx: RawSender<ResultMap<Id<Setter>, (), Error>>,
    },
    RegisterChannelWatch {
        watch: Vec<(Vec<GetterSelector>, Exactly<Range>)>,
        on_event: Box<ExtSender<WatchEvent>>,
        tx: RawSender<(Box<ExtSender<WatchEvent>>, usize, Arc<AtomicBool>)>
    },
    UnregisterChannelWatch {
        key: usize
    },
}


impl AdapterManagerState {
    pub fn execute(&mut self, op: Op) {
        use self::Op::*;
        match op {
            AddAdapter { adapter, tx } => {
                let _ = tx.send(self.add_adapter(adapter));
            },
            AddService { service, tx } => {
                let _ = tx.send(self.add_service(service));
            },
            AddGetter { getter, tx } => {
                let _ = tx.send(self.add_getter(getter));
            },
            AddSetter { setter, tx } => {
                let _ = tx.send(self.add_setter(setter));
            },
            RemoveAdapter { id, tx } => {
                let _ = tx.send(self.remove_adapter(id));
            },
            RemoveService { id, tx } => {
                let _ = tx.send(self.remove_service(id));
            },
            RemoveGetter { id, tx } => {
                let _ = tx.send(self.remove_getter(id));
            },
            RemoveSetter { id, tx } => {
                let _ = tx.send(self.remove_setter(id));
            },
            GetServices { selectors, tx } => {
                let _ = tx.send(self.get_services(selectors));
            },
            GetGetterChannels { selectors, tx } => {
                let _ = tx.send(self.get_getter_channels(selectors));
            },
            GetSetterChannels { selectors, tx } => {
                let _ = tx.send(self.get_setter_channels(selectors));
            },
            AddServiceTags { selectors, tags, tx } => {
                let _ = tx.send(self.add_service_tags(selectors, tags));
            },
            AddGetterTags { selectors, tags, tx } => {
                let _ = tx.send(self.add_getter_tags(selectors, tags));
            },
            AddSetterTags { selectors, tags, tx } => {
                let _ = tx.send(self.add_setter_tags(selectors, tags));
            },
            RemoveServiceTags { selectors, tags, tx } => {
                let _ = tx.send(self.remove_service_tags(selectors, tags));
            },
            RemoveGetterTags { selectors, tags, tx } => {
                let _ = tx.send(self.remove_getter_tags(selectors, tags));
            },
            RemoveSetterTags { selectors, tags, tx } => {
                let _ = tx.send(self.remove_setter_tags(selectors, tags));
            },
            FetchValues { selectors, tx } => {
                let _ = tx.send(self.fetch_values(selectors));
            },
            SendValues { keyvalues, tx } => {
                let _ = tx.send(self.send_values(keyvalues));
            },
            RegisterChannelWatch { watch, on_event, tx } => {
                let _ = tx.send(self.register_channel_watch(watch, on_event));
            },
            UnregisterChannelWatch { key } => {
                // Silent method, no result.
                self.unregister_channel_watch(key);
            },
        }
    }
}

impl AdapterManagerState {
    pub fn new() -> Self {
        AdapterManagerState {
           adapter_by_id: HashMap::new(),
           service_by_id: HashMap::new(),
           getter_by_id: HashMap::new(),
           setter_by_id: HashMap::new(),
           watchers: Arc::new(Mutex::new(WatchMap::new())),
       }
    }

    /// Add an adapter to the system.
    ///
    /// # Errors
    ///
    /// Returns an error if an adapter with the same id is already present.
    pub fn add_adapter(&mut self, adapter: Box<Adapter>) -> Result<(), Error> {
        match self.adapter_by_id.entry(adapter.id()) {
            Entry::Occupied(_) => return Err(Error::InternalError(InternalError::DuplicateAdapter(adapter.id()))),
            Entry::Vacant(entry) => {
                entry.insert(AdapterData::new(adapter));
            }
        }
        Ok(())
    }

    /// Remove an adapter from the system, including all its services and channels.
    ///
    /// # Errors
    ///
    /// Returns an error if no adapter with this identifier exists. Otherwise, attempts
    /// to cleanup as much as possible, even if for some reason the system is in an
    /// inconsistent state.
    fn remove_adapter(&mut self, id: Id<AdapterId>) -> Result<(), Error> {
        let mut services = match self.adapter_by_id.remove(&id) {
            Some(AdapterData {services: adapter_services, ..}) => {
                adapter_services
            }
            None => return Err(Error::InternalError(InternalError::NoSuchAdapter(id))),
        };
        for (service_id, _) in services.drain() {
            let _ignored = self.aux_remove_service(&service_id);
        }
        Ok(())
    }

    /// Add a service to the system. Called by the adapter when a new
    /// service (typically a new device) has been detected/configured.
    ///
    /// The `service` must NOT have any channels yet. Channels must be added through
    /// `add_channel`.
    ///
    /// # Requirements
    ///
    /// The adapter is in charge of making sure that identifiers persist across reboots.
    ///
    /// # Errors
    ///
    /// Returns an error if any of:
    /// - `service` has channels;
    /// - a service with id `service.id` is already installed on the system;
    /// - there is no adapter with id `service.adapter`.
    fn add_service(&mut self, service: Service) -> Result<(), Error> {
        // Make sure that there are no channels.
        if !service.getters.is_empty() || !service.setters.is_empty() {
            return Err(Error::InternalError(InternalError::InvalidInitialService));
        }
        let mut services_for_this_adapter =
            match self.adapter_by_id.get_mut(&service.adapter) {
                None => return Err(Error::InternalError(InternalError::NoSuchAdapter(service.adapter.clone()))),
                Some(&mut AdapterData {ref mut services, ..}) => {
                    services
                }
            };
        let id = service.id.clone();
        let service = Rc::new(RefCell::new(service));
        let insert_in_adapters =
            match InsertInMap::start(&mut services_for_this_adapter, vec![(id.clone(), service.clone())]) {
                Err(k) => return Err(Error::InternalError(InternalError::DuplicateService(k))),
                Ok(transaction) => transaction
            };

        let insert_in_services =
            match InsertInMap::start(&mut self.service_by_id, vec![(id.clone(), service)]) {
                Err(k) => return Err(Error::InternalError(InternalError::DuplicateService(k))),
                Ok(transaction) => transaction
            };

        // If we haven't bailed out yet, leave all this stuff in the maps and sets.
        insert_in_adapters.commit();
        insert_in_services.commit();
        Ok(())
    }

    /// Remove a service previously registered on the system. Typically, called by
    /// an adapter when a service (e.g. a device) is disconnected.
    ///
    /// # Errors
    ///
    /// Returns an error if any of:
    /// - there is no such service;
    /// - there is an internal inconsistency, in which case this method will still attempt to
    /// cleanup before returning an error.
    fn remove_service(&mut self, service_id: Id<ServiceId>) -> Result<(), Error> {
        let adapter = try!(self.aux_remove_service(&service_id));
        match self.adapter_by_id.get_mut(&adapter) {
            None => Err(Error::InternalError(InternalError::NoSuchAdapter(adapter.clone()))),
            Some(mut data) => {
                if data.services.remove(&service_id).is_none() {
                    Err(Error::InternalError(InternalError::NoSuchService(service_id)))
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Add a getter to the system. Typically, this is called by the adapter when a new
    /// service has been detected/configured. Some services may gain/lose getters at
    /// runtime depending on their configuration.
    ///
    /// # Requirements
    ///
    /// The adapter is in charge of making sure that identifiers persist across reboots.
    ///
    /// # Errors
    ///
    /// Returns an error if the adapter is not registered, the parent service is not
    /// registered, or a channel with the same identifier is already registered.
    /// In either cases, this method reverts all its changes.
    fn add_getter(&mut self, getter: Channel<Getter>) -> Result<(), Error> {
        let getter_by_id = &mut self.getter_by_id;
        let service = match self.service_by_id.get_mut(&getter.service) {
            None => return Err(Error::InternalError(InternalError::NoSuchService(getter.service.clone()))),
            Some(service) => service
        };
        if service.borrow().adapter != getter.adapter {
            return Err(Error::InternalError(InternalError::ConflictingAdapter(service.borrow().adapter.clone(), getter.adapter.clone())));
        }
        let getters = &mut service.borrow_mut().getters;

        let insert_in_service = match InsertInMap::start(getters, vec![(getter.id.clone(), getter.clone())]) {
            Ok(transaction) => transaction,
            Err(id) => return Err(Error::InternalError(InternalError::DuplicateGetter(id)))
        };

        /*
                // FIXME: Check whether we match an ongoing watcher.
                for watcher in &mut watchers.lock().unwrap().watchers.values() {
                    let matches = watcher.selectors.iter().find(|selector| {
                        getter_data.matches(selector)
                    }).is_some();
                    if matches {
                        getter_data.watchers.insert(watcher.clone());
                        watcher.push_getter(&getter_data.id);
                        // FIXME: Notify WatchEvent of topology change
                        // FIXME: register_single_channel_watch_values
                    };
                }
        */
        let getter_data = GetterData::new(getter);
        let insert_in_getters = match InsertInMap::start(getter_by_id, vec![(getter_data.id.clone(), getter_data)]) {
            Ok(transaction) => transaction,
            Err(id) => return Err(Error::InternalError(InternalError::DuplicateGetter(id)))
        };

        insert_in_service.commit();
        insert_in_getters.commit();
        Ok(())
    }

    /// Remove a setter previously registered on the system. Typically, called by
    /// an adapter when a service is reconfigured to remove one of its setters.
    ///
    /// # Error
    ///
    /// This method returns an error if the setter is not registered or if the service
    /// is not registered. In either case, it attemps to clean as much as possible, even
    /// if the state is inconsistent.
    fn remove_getter(&mut self, id: Id<Getter>) -> Result<(), Error> {
        let getter = match self.getter_by_id.remove(&id) {
            None => return Err(Error::InternalError(InternalError::NoSuchGetter(id))),
            Some(getter) => getter
        };
        match self.service_by_id.get_mut(&getter.getter.service) {
            None => Err(Error::InternalError(InternalError::NoSuchService(getter.getter.service))),
            Some(service) => {
                if service.borrow_mut().getters.remove(&id).is_none() {
                    Err(Error::InternalError(InternalError::NoSuchGetter(id)))
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Add a setter to the system. Typically, this is called by the adapter when a new
    /// service has been detected/configured. Some services may gain/lose setters at
    /// runtime depending on their configuration.
    ///
    /// # Requirements
    ///
    /// The adapter is in charge of making sure that identifiers persist across reboots.
    ///
    /// # Errors
    ///
    /// Returns an error if the adapter is not registered, the parent service is not
    /// registered, or a channel with the same identifier is already registered.
    /// In either cases, this method reverts all its changes.
    fn add_setter(&mut self, setter: Channel<Setter>) -> Result<(), Error> {
        let service = match self.service_by_id.get_mut(&setter.service) {
            None => return Err(Error::InternalError(InternalError::NoSuchService(setter.service.clone()))),
            Some(service) => service
        };
        if service.borrow().adapter != setter.adapter {
            return Err(Error::InternalError(InternalError::ConflictingAdapter(service.borrow().adapter.clone(), setter.adapter)));
        }
        let setters = &mut service.borrow_mut().setters;
        let insert_in_service = match InsertInMap::start(setters, vec![(setter.id.clone(), setter.clone())]) {
            Ok(transaction) => transaction,
            Err(id) => return Err(Error::InternalError(InternalError::DuplicateSetter(id)))
        };
        let insert_in_setters = match InsertInMap::start(&mut self.setter_by_id, vec![(setter.id.clone(), SetterData::new(setter))]) {
            Ok(transaction) => transaction,
            Err(id) => return Err(Error::InternalError(InternalError::DuplicateSetter(id)))
        };
        insert_in_service.commit();
        insert_in_setters.commit();
        Ok(())
    }

    /// Remove a setter previously registered on the system. Typically, called by
    /// an adapter when a service is reconfigured to remove one of its setters.
    ///
    /// # Error
    ///
    /// This method returns an error if the setter is not registered or if the service
    /// is not registered. In either case, it attemps to clean as much as possible, even
    /// if the state is inconsistent.
    fn remove_setter(&mut self, id: Id<Setter>) -> Result<(), Error> {
        let setter = match self.setter_by_id.remove(&id) {
            None => return Err(Error::InternalError(InternalError::NoSuchSetter(id))),
            Some(setter) => setter
        };
        match self.service_by_id.get_mut(&setter.setter.service) {
            None => Err(Error::InternalError(InternalError::NoSuchService(setter.setter.service))),
            Some(service) => {
                if service.borrow_mut().setters.remove(&id).is_none() {
                    Err(Error::InternalError(InternalError::NoSuchSetter(id)))
                } else {
                    Ok(())
                }
            }
        }
    }

    fn get_services(&self, selectors: Vec<ServiceSelector>) -> Vec<Service> {
        // This implementation is not nearly optimal, but it should be sufficient in a system
        // with relatively few services.
        let mut result = Vec::new();
        self.with_services(selectors, |service| {
            result.push(service.borrow().clone())
        });
        result
    }

    fn add_service_tags(&self, selectors: Vec<ServiceSelector>, tags: Vec<Id<TagId>>) -> usize {
        let mut result = 0;
        self.with_services(selectors, |service| {
            let tag_set = &mut service.borrow_mut().tags;
            for tag in &tags {
                let _ = tag_set.insert(tag.clone());
            }
            result += 1;
        });
        result
    }

    fn remove_service_tags(&self, selectors: Vec<ServiceSelector>, tags: Vec<Id<TagId>>) -> usize {
        let mut result = 0;
        self.with_services(selectors, |service| {
            let tag_set = &mut service.borrow_mut().tags;
            for tag in &tags {
                let _ = tag_set.remove(&tag);
            }
            result += 1;
        });
        result
    }

    fn get_getter_channels(&self, selectors: Vec<GetterSelector>) -> Vec<Channel<Getter>>
    {
        Self::aux_get_channels(selectors, &self.getter_by_id)
    }
    fn get_setter_channels(&self, selectors: Vec<SetterSelector>) -> Vec<Channel<Setter>>
    {
        Self::aux_get_channels(selectors, &self.setter_by_id)
    }


    fn add_getter_tags(&mut self, selectors: Vec<GetterSelector>, tags: Vec<Id<TagId>>) -> usize {
        Self::aux_add_channel_tags(selectors, tags, &mut self.getter_by_id)
    }
    fn add_setter_tags(&mut self, selectors: Vec<SetterSelector>, tags: Vec<Id<TagId>>) -> usize {
        Self::aux_add_channel_tags(selectors, tags, &mut self.setter_by_id)
    }
    fn remove_getter_tags(&mut self, selectors: Vec<GetterSelector>, tags: Vec<Id<TagId>>) -> usize {
        Self::aux_remove_channel_tags(selectors, tags, &mut self.getter_by_id)
    }
    fn remove_setter_tags(&mut self, selectors: Vec<SetterSelector>, tags: Vec<Id<TagId>>) -> usize {
        Self::aux_remove_channel_tags(selectors, tags, &mut self.setter_by_id)
    }

    /// Read the latest value from a set of channels
    fn fetch_values(&mut self, selectors: Vec<GetterSelector>) -> ResultSet<Id<Getter>, Option<Value>, Error> {
        // First group per adapter, so as to let adapters optimize fetches.
        let mut per_adapter = HashMap::new();
        Self::with_channels(selectors, &self.getter_by_id, |data| {
            use std::collections::hash_map::Entry::*;
            match per_adapter.entry(data.getter.adapter.clone()) {
                Vacant(entry) => {
                    entry.insert(vec![data.getter.id.clone()]);
                }
                Occupied(mut entry) => {
                    entry.get_mut().push(data.getter.id.clone());
                }
            }
        });

        // Now fetch the values
        let mut results = vec![];
        for (adapter_id, getters) in per_adapter {
            match self.adapter_by_id.get(&adapter_id) {
                None => {}, // Internal inconsistency. FIXME: Log this somewhere.
                Some(ref adapter_data) => {
                    let mut got = adapter_data
                        .adapter
                        .fetch_values(getters);

                    results.append(&mut got);
                }
            }
        }
        results
    }

    /// Send values to a set of channels
    fn send_values(&self, mut keyvalues: Vec<(Vec<SetterSelector>, Value)>) -> ResultMap<Id<Setter>, (), Error> {
        // First determine the channels and group them by adapter.
        let mut per_adapter = HashMap::new();
        for (selectors, value) in keyvalues.drain(..) {
            Self::with_channels(selectors, &self.setter_by_id, |data| {
                use std::collections::hash_map::Entry::*;
                match per_adapter.entry(data.setter.adapter.clone()) {
                    Vacant(entry) => {
                        entry.insert(vec![(data.setter.id.clone(), value.clone())]);
                    }
                    Occupied(mut entry) => {
                        entry.get_mut().push((data.setter.id.clone(), value.clone()));
                    }
                }
            })
        }


        // Dispatch to adapter
        let mut results = Vec::new();
        for (adapter_id, payload) in per_adapter.drain() {
            let adapter = match self.adapter_by_id.get(&adapter_id) {
                None => continue, // That's an internal inconsistency. FIXME: Log this somewhere.
                Some(adapter) => adapter
            };
            let mut got = adapter.adapter.send_values(payload);
            results.append(&mut got);
        }

        results
    }


    fn register_channel_watch(&mut self, mut watch: Vec<(Vec<GetterSelector>, Exactly<Range>)>, on_event: Box<ExtSender<WatchEvent>>) -> (Box<ExtSender<WatchEvent>>, usize, Arc<AtomicBool>) {
        // Store the watcher. This will serve when we new channels are added, to hook them up
        // to this watcher.
        let is_dropped = Arc::new(AtomicBool::new(false));
        let watcher = self.watchers.lock().unwrap().create(watch.clone(),
            &is_dropped, on_event.clone());
        let key = watcher.key;

        // Regroup per adapter.
        let mut per_adapter = HashMap::new();

        for (selectors, filter) in watch.drain(..) {
            // Find out which channels already match the selectors and attach
            // the watcher immediately.
            let filter = &filter;
            Self::with_channels_mut(selectors, &mut self.getter_by_id, |mut getter_data| {
                use std::collections::hash_map::Entry::*;
                getter_data.watchers.insert(watcher.clone());
                watcher.push_getter(&getter_data.id);

                let range = match *filter {
                    Exactly::Exactly(ref range) => Some(range.clone()),
                    Exactly::Always => None,
                    _ => return // Don't watch data, just topology.
                };

                let data = (getter_data.id.clone(), range);
                let adapter = getter_data.adapter.clone();
                match per_adapter.entry(adapter) {
                    Vacant(entry) => {
                        entry.insert(vec![data]);
                    },
                    Occupied(mut entry) => entry.get_mut().push(data),
                }
            });
        }

        // Now dispatch to adapters.
        for (adapter_id, request) in per_adapter.drain() {
            let adapter = match self.adapter_by_id.get(&adapter_id) {
                None => {
                    debug_assert!(false, "We have a registered channel whose adapter is not registered: {:?}", adapter_id);
                    // FIXME: Logging would be nice.
                    continue
                },
                Some(adapter) => adapter
            };

            let is_dropped = is_dropped.clone();
            let on_err = on_event.clone();
            let on_ok = on_event.filter_map(move |event| {
                if is_dropped.load(Ordering::Relaxed) {
                    return None;
                }
                Some(match event {
                    AdapterWatchEvent::Enter { id, value } =>
                        WatchEvent::EnterRange {
                            from: id,
                            value: value
                        },
                    AdapterWatchEvent::Exit { id, value } =>
                        WatchEvent::ExitRange {
                            from: id,
                            value: value
                        },
                })
            });

            let watcher = watcher.clone();
            for (id, result) in adapter.register_watch(request, Box::new(on_ok)) {
                match result {
                    Err(err) => {
                        let event = WatchEvent::InitializationError {
                            channel: id.clone(),
                            error: err
                        };
                        let _ = on_err.send(event);
                    },
                    Ok(guard) => watcher.push_guard(guard)
                }
            }
        }

        // Upon drop, this data structure will immediately drop `is_dropped` and then dispatch
        // `unregister_channel_watch` to unregister everything else.
        (on_event, key, is_dropped)
    }

    /// Unregister a watch previously registered with `register_channel_watch`.
    ///
    /// This method is dispatched from `WatchGuard::drop()`.
    pub fn unregister_channel_watch(&mut self, key: usize) { // FIXME: Use a better type than `usize`
        // Remove `key` from `watchers`. This will prevent the watcher from being registered
        // automatically with any new getter.
        let mut watcher_data = match self.watchers.lock().unwrap().remove(key) {
            None => {
                debug_assert!(false, "Attempting to unregister a watcher that has already been removed {}", key);
                return
            } // FIXME: Logging would be nice.
            Some(watcher_data) => watcher_data
        };

        // Remove the watcher from all getters.
        for getter_id in watcher_data.getters.borrow().iter() {
            let mut getter = match self.getter_by_id.get_mut(getter_id) {
                None => return, // Race condition between removing the getter and dropping the watcher.
                Some(getter) => getter
            };
            if !getter.watchers.remove(&watcher_data) {
                debug_assert!(false, "Attempting to unregister a watcher that has already been removed from its getter {}, {:?}", key, getter_id);
            }
        }

        // Sanity check
        debug_assert!(Arc::get_mut(&mut watcher_data).is_none(),
            "We still have dangling pointers to a watcher. That's not good");

        // At this stage, `watcher_data` has no reference left. All its `guards` will be dropped.
    }
}
