//! Implement thread-local storage.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::collections::HashSet;

use log::trace;

use rustc_middle::ty;
use rustc_target::abi::{Size, HasDataLayout};

use crate::{
    HelpersEvalContextExt, InterpResult, MPlaceTy, Scalar, StackPopCleanup, Tag, ThreadId,
    ThreadsEvalContextExt,
};

pub type TlsKey = u128;

#[derive(Clone, Debug)]
pub struct TlsEntry<'tcx> {
    /// The data for this key. None is used to represent NULL.
    /// (We normalize this early to avoid having to do a NULL-ptr-test each time we access the data.)
    data: BTreeMap<ThreadId, Scalar<Tag>>,
    dtor: Option<ty::Instance<'tcx>>,
}

#[derive(Debug)]
pub struct TlsData<'tcx> {
    /// The Key to use for the next thread-local allocation.
    next_key: TlsKey,

    /// pthreads-style thread-local storage.
    keys: BTreeMap<TlsKey, TlsEntry<'tcx>>,

    /// A single per thread destructor of the thread local storage (that's how
    /// things work on macOS) with a data argument.
    thread_dtors: BTreeMap<ThreadId, (ty::Instance<'tcx>, Scalar<Tag>)>,

    /// Whether we are in the "destruct" phase, during which some operations are UB.
    dtors_running: HashSet<ThreadId>,

    /// The last TlsKey used to retrieve a TLS destructor.
    last_dtor_key: BTreeMap<ThreadId, TlsKey>,
}

impl<'tcx> Default for TlsData<'tcx> {
    fn default() -> Self {
        TlsData {
            next_key: 1, // start with 1 as we must not use 0 on Windows
            keys: Default::default(),
            thread_dtors: Default::default(),
            dtors_running: Default::default(),
            last_dtor_key: Default::default(),
        }
    }
}

impl<'tcx> TlsData<'tcx> {
    /// Generate a new TLS key with the given destructor.
    /// `max_size` determines the integer size the key has to fit in.
    pub fn create_tls_key(&mut self, dtor: Option<ty::Instance<'tcx>>, max_size: Size) -> InterpResult<'tcx, TlsKey> {
        let new_key = self.next_key;
        self.next_key += 1;
        self.keys.insert(new_key, TlsEntry { data: Default::default(), dtor }).unwrap_none();
        trace!("New TLS key allocated: {} with dtor {:?}", new_key, dtor);

        if max_size.bits() < 128 && new_key >= (1u128 << max_size.bits() as u128) {
            throw_unsup_format!("we ran out of TLS key space");
        }
        Ok(new_key)
    }

    pub fn delete_tls_key(&mut self, key: TlsKey) -> InterpResult<'tcx> {
        match self.keys.remove(&key) {
            Some(_) => {
                trace!("TLS key {} removed", key);
                Ok(())
            }
            None => throw_ub_format!("removing a non-existig TLS key: {}", key),
        }
    }

    pub fn load_tls(
        &self,
        key: TlsKey,
        thread_id: ThreadId,
        cx: &impl HasDataLayout,
    ) -> InterpResult<'tcx, Scalar<Tag>> {
        match self.keys.get(&key) {
            Some(TlsEntry { data, .. }) => {
                let value = data.get(&thread_id).copied();
                trace!("TLS key {} for thread {:?} loaded: {:?}", key, thread_id, value);
                Ok(value.unwrap_or_else(|| Scalar::null_ptr(cx).into()))
            }
            None => throw_ub_format!("loading from a non-existing TLS key: {}", key),
        }
    }

    pub fn store_tls(
        &mut self,
        key: TlsKey,
        thread_id: ThreadId,
        new_data: Option<Scalar<Tag>>
    ) -> InterpResult<'tcx> {
        match self.keys.get_mut(&key) {
            Some(TlsEntry { data, .. }) => {
                match new_data {
                    Some(scalar) => {
                        trace!("TLS key {} for thread {:?} stored: {:?}", key, thread_id, scalar);
                        data.insert(thread_id, scalar);
                    }
                    None => {
                        trace!("TLS key {} for thread {:?} removed", key, thread_id);
                        data.remove(&thread_id);
                    }
                }
                Ok(())
            }
            None => throw_ub_format!("storing to a non-existing TLS key: {}", key),
        }
    }

    /// Set the thread wide destructor of the thread local storage for the given
    /// thread. This function is used to implement `_tlv_atexit` shim on MacOS.
    ///
    /// Thread wide dtors are available only on MacOS. There is one destructor
    /// per thread as can be guessed from the following comment in the
    /// [`_tlv_atexit`
    /// implementation](https://github.com/opensource-apple/dyld/blob/195030646877261f0c8c7ad8b001f52d6a26f514/src/threadLocalVariables.c#L389):
    ///
    ///     // NOTE: this does not need locks because it only operates on current thread data
    pub fn set_thread_dtor(
        &mut self,
        thread: ThreadId,
        dtor: ty::Instance<'tcx>,
        data: Scalar<Tag>
    ) -> InterpResult<'tcx> {
        if self.dtors_running.contains(&thread) {
            // UB, according to libstd docs.
            throw_ub_format!("setting thread's local storage destructor while destructors are already running");
        }
        if self.thread_dtors.insert(thread, (dtor, data)).is_some() {
            throw_unsup_format!("setting more than one thread local storage destructor for the same thread is not supported");
        }
        Ok(())
    }

    /// Returns a dtor, its argument and its index, if one is supposed to run.
    /// `key` is the last dtors that was run; we return the *next* one after that.
    ///
    /// An optional destructor function may be associated with each key value.
    /// At thread exit, if a key value has a non-NULL destructor pointer,
    /// and the thread has a non-NULL value associated with that key,
    /// the value of the key is set to NULL, and then the function pointed
    /// to is called with the previously associated value as its sole argument.
    /// The order of destructor calls is unspecified if more than one destructor
    /// exists for a thread when it exits.
    ///
    /// If, after all the destructors have been called for all non-NULL values
    /// with associated destructors, there are still some non-NULL values with
    /// associated destructors, then the process is repeated.
    /// If, after at least {PTHREAD_DESTRUCTOR_ITERATIONS} iterations of destructor
    /// calls for outstanding non-NULL values, there are still some non-NULL values
    /// with associated destructors, implementations may stop calling destructors,
    /// or they may continue calling destructors until no non-NULL values with
    /// associated destructors exist, even though this might result in an infinite loop.
    fn fetch_tls_dtor(
        &mut self,
        key: Option<TlsKey>,
        thread_id: ThreadId,
    ) -> Option<(ty::Instance<'tcx>, Scalar<Tag>, TlsKey)> {
        use std::collections::Bound::*;

        let thread_local = &mut self.keys;
        let start = match key {
            Some(key) => Excluded(key),
            None => Unbounded,
        };
        for (&key, TlsEntry { data, dtor }) in
            thread_local.range_mut((start, Unbounded))
        {
            match data.entry(thread_id) {
                Entry::Occupied(entry) => {
                    let data_scalar = entry.remove();
                    if let Some(dtor) = dtor {
                        let ret = Some((*dtor, data_scalar, key));
                        return ret;
                    }
                }
                Entry::Vacant(_) => {}
            }
        }
        None
    }
}

impl<'mir, 'tcx: 'mir> EvalContextPrivExt<'mir, 'tcx> for crate::MiriEvalContext<'mir, 'tcx> {}
trait EvalContextPrivExt<'mir, 'tcx: 'mir>: crate::MiriEvalContextExt<'mir, 'tcx> {
    /// Schedule TLS destructors for the main thread on Windows. The
    /// implementation assumes that we do not support concurrency on Windows
    /// yet.
    fn schedule_windows_tls_dtors(&mut self) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        let active_thread = this.get_active_thread()?;
        assert_eq!(this.get_total_thread_count()?, 1, "concurrency on Windows not supported");
        this.machine.tls.dtors_running.insert(active_thread);
        // Windows has a special magic linker section that is run on certain events.
        // Instead of searching for that section and supporting arbitrary hooks in there
        // (that would be basically https://github.com/rust-lang/miri/issues/450),
        // we specifically look up the static in libstd that we know is placed
        // in that section.
        let thread_callback = this.eval_path_scalar(&["std", "sys", "windows", "thread_local", "p_thread_callback"])?;
        let thread_callback = this.memory.get_fn(thread_callback.not_undef()?)?.as_instance()?;

        // The signature of this function is `unsafe extern "system" fn(h: c::LPVOID, dwReason: c::DWORD, pv: c::LPVOID)`.
        let reason = this.eval_path_scalar(&["std", "sys", "windows", "c", "DLL_PROCESS_DETACH"])?;
        let ret_place = MPlaceTy::dangling(this.machine.layouts.unit, this).into();
        this.call_function(
            thread_callback,
            &[Scalar::null_ptr(this).into(), reason.into(), Scalar::null_ptr(this).into()],
            Some(ret_place),
            StackPopCleanup::None { cleanup: true },
        )?;

        this.enable_thread(active_thread)?;
        Ok(())
    }

    /// Schedule the MacOS thread destructor of the thread local storage to be
    /// executed.
    ///
    /// Note: It is safe to call this function also on other Unixes.
    fn schedule_macos_tls_dtor(&mut self) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        let thread_id = this.get_active_thread()?;
        if let Some((instance, data)) = this.machine.tls.thread_dtors.remove(&thread_id) {
            trace!("Running macos dtor {:?} on {:?} at {:?}", instance, data, thread_id);

            let ret_place = MPlaceTy::dangling(this.machine.layouts.unit, this).into();
            this.call_function(
                instance,
                &[data.into()],
                Some(ret_place),
                StackPopCleanup::None { cleanup: true },
            )?;

            // Enable the thread so that it steps through the destructor which
            // we just scheduled. Since we deleted the destructor, it is
            // guaranteed that we will schedule it again. The `dtors_running`
            // flag will prevent the code from adding the destructor again.
            this.enable_thread(thread_id)?;
        }
        Ok(())
    }

    /// Schedule a pthread TLS destructor.
    fn schedule_pthread_tls_dtors(&mut self) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        let active_thread = this.get_active_thread()?;

        assert!(this.has_terminated(active_thread)?, "running TLS dtors for non-terminated thread");
        // Fetch next dtor after `key`.
        let last_key = this.machine.tls.last_dtor_key.get(&active_thread).cloned();
        let dtor = match this.machine.tls.fetch_tls_dtor(last_key, active_thread) {
            dtor @ Some(_) => dtor,
            // We ran each dtor once, start over from the beginning.
            None => {
                this.machine.tls.fetch_tls_dtor(None, active_thread)
            }
        };
        if let Some((instance, ptr, key)) = dtor {
            this.machine.tls.last_dtor_key.insert(active_thread, key);
            trace!("Running TLS dtor {:?} on {:?} at {:?}", instance, ptr, active_thread);
            assert!(!this.is_null(ptr).unwrap(), "data can't be NULL when dtor is called!");

            let ret_place = MPlaceTy::dangling(this.machine.layouts.unit, this).into();
            this.call_function(
                instance,
                &[ptr.into()],
                Some(ret_place),
                StackPopCleanup::None { cleanup: true },
            )?;

            this.enable_thread(active_thread)?;
            return Ok(());
        }
        this.machine.tls.last_dtor_key.remove(&active_thread);

        Ok(())
    }
}

impl<'mir, 'tcx: 'mir> EvalContextExt<'mir, 'tcx> for crate::MiriEvalContext<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: crate::MiriEvalContextExt<'mir, 'tcx> {

    /// Schedule an active thread's TLS destructor to run on the active thread.
    /// Note that this function does not run the destructors itself, it just
    /// schedules them one by one each time it is called and reenables the
    /// thread so that it can be executed normally by the main execution loop.
    ///
    /// FIXME: we do not support yet deallocation of thread local statics.
    /// Issue: https://github.com/rust-lang/miri/issues/1369
    fn schedule_next_tls_dtor_for_active_thread(&mut self) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        let active_thread = this.get_active_thread()?;

        if this.tcx.sess.target.target.target_os == "windows" {
            if !this.machine.tls.dtors_running.contains(&active_thread) {
                this.machine.tls.dtors_running.insert(active_thread);
                this.schedule_windows_tls_dtors()?;
            }
        } else {
            this.machine.tls.dtors_running.insert(active_thread);
            // The macOS thread wide destructor runs "before any TLS slots get
            // freed", so do that first.
            this.schedule_macos_tls_dtor()?;
            this.schedule_pthread_tls_dtors()?;
        }

        Ok(())
    }
}
