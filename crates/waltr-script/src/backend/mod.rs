mod vm;

use crate::{
    Error, ExecutionReport, HostContext, HostDispatcher, HostRequest, StructuredValue,
    host::CallbackRequest,
};
use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use std::{collections::BTreeMap, sync::Arc, time::Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // One owner pump keeps teardown lexical.
pub(crate) async fn run(
    source: String,
    input: StructuredValue,
    host: Arc<dyn HostDispatcher>,
    context: HostContext,
    cancel: CancellationToken,
    start: Instant,
) -> ExecutionReport {
    let limits = context.limits.clone();
    let mut vm = match vm::Vm::new(limits.clone(), cancel.clone(), start) {
        Ok(vm) => vm,
        Err(error) => return ExecutionReport::new(Err(error), 0, start, false),
    };
    let result = async {
        vm.compile(&source)?;
        vm.start(&input)?;
        let (send, mut receive) = mpsc::channel::<CallbackRequest>(limits.in_flight);
        let mut work = FuturesUnordered::new();
        let mut validating = BTreeMap::<u32, CallbackRequest>::new();
        let mut callbacks = 0;
        let mut completion = None;
        let mut callback_request = None;
        loop {
            vm.check()?;
            // Bounded job batches give completions and callback requests a turn.
            for _ in 0..8 {
                let state = vm.zero("step")?;
                if state < 0 {
                    return Err(vm.script_error()?);
                }
                if state == 0 {
                    break;
                }
            }
            vm.failure()?;
            for _ in 0..8 {
                let id = vm.zero("event")?;
                if id == 0 {
                    break;
                }
                if id < 0 || work.len() >= limits.in_flight {
                    return Err(Error::Limit);
                }
                let id = u32::try_from(id).map_err(|_| Error::Protocol)?;
                let operation = vm.operation(id)?;
                let arguments = vm.arguments(id)?;
                let bytes = vm.bytes(id)?;
                let generation = vm.one("event_handle", id)?;
                let handle = if generation == 0 {
                    None
                } else {
                    Some(context.lookup(u32::try_from(generation).map_err(|_| Error::Protocol)?)?)
                };
                let callback = if vm.one("event_callback", id)? == 1 {
                    let send = send.clone();
                    Some(context.callback(move |value| {
                        let send = send.clone();
                        Box::pin(async move {
                            let (reply, receive) = tokio::sync::oneshot::channel();
                            send.send(CallbackRequest { id, value, reply })
                                .await
                                .map_err(|_| Error::Identity)?;
                            receive.await.map_err(|_| Error::Cancelled)?
                        })
                    }))
                } else {
                    None
                };
                if vm.one("event_pop", id)? != 0 {
                    return Err(Error::Protocol);
                }
                vm.check()?;
                let future = host.dispatch(
                    HostRequest {
                        operation,
                        arguments,
                        bytes,
                        callback,
                        handle,
                    },
                    context.clone(),
                );
                work.push(async move { (id, future.await) });
            }
            let ids: Vec<u32> = validating.keys().copied().collect();
            for id in ids {
                let state = vm.one("callback_status", id)?;
                if state != 0 {
                    let request = validating.remove(&id).ok_or(Error::Protocol)?;
                    if state < 0 {
                        let error = vm.callback_error(id)?;
                        let _sent = request.reply.send(Err(error));
                        return Err(error);
                    }
                    if vm.one("callback_result", id)? != 0 {
                        return Err(vm.script_error()?);
                    }
                    let _sent = request.reply.send(vm.output());
                }
            }
            match vm.zero("status")? {
                1 => {
                    if vm.zero("result")? != 0 {
                        return Err(vm.script_error()?);
                    }
                    return vm.output();
                }
                n if n < 0 => return Err(vm.script_error()?),
                _ => {}
            }
            // One ready item from each queue per turn, independent of select order.
            if completion.is_none() {
                completion = work.next().now_or_never().flatten();
            }
            if callback_request.is_none() {
                callback_request = receive.try_recv().ok();
            }
            let completion_progress = if let Some((id, reply)) = completion.take() {
                if validating.contains_key(&id) {
                    return Err(Error::Protocol);
                }
                vm.settle(id, reply, &context)?;
                true
            } else {
                false
            };
            let callback_progress = if let Some(request) = callback_request.take() {
                callbacks += 1;
                if callbacks > limits.callbacks || validating.len() >= limits.in_flight {
                    return Err(Error::Limit);
                }
                if let std::collections::btree_map::Entry::Vacant(slot) =
                    validating.entry(request.id)
                    && vm.callback(request.id, &request.value)?
                {
                    slot.insert(request);
                } else {
                    let _sent = request.reply.send(Err(Error::Identity));
                }
                true
            } else {
                false
            };
            if completion_progress || callback_progress {
                continue;
            }
            tokio::select! {
                () = cancel.cancelled() => return Err(Error::Cancelled),
                item = work.next(), if !work.is_empty() => { completion=item; }
                request = receive.recv() => { callback_request=request; }
                () = tokio::time::sleep(std::time::Duration::from_millis(1)) => {}
            }
        }
    }
    .await;
    // The future above owns all host work and callback responders; they drop here.
    let fuel = vm.fuel();
    let _guest_cleanup = vm.cleanup();
    // Store destruction frees all guest memory, even if a fuel trap broke C teardown.
    drop(vm);
    ExecutionReport::new(result, fuel, start, true)
}

pub(crate) fn validate(source: &str, limits: crate::Limits) -> Result<(), Error> {
    let mut vm = vm::Vm::new(limits, CancellationToken::new(), Instant::now())?;
    let result = vm.compile(source);
    // Store destruction frees linear memory even after a trap during compilation.
    let _cleaned = vm.cleanup();
    result
}
