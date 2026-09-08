use super::*;

pub(super) fn schedule_shm_fallback(
    event_loop: &LoopHandle<'static, State>,
    inner: &mut CastInner,
    stream_id: CastStreamId,
) {
    if mem::replace(&mut inner.dma_failed, true) {
        return;
    }
    inner.is_active = false;
    inner.waiting_for_buffer = false;
    // Leave buffer ownership with PipeWire until remove_buffer retires the old pool.
    event_loop.insert_idle(move |state| {
        for cast in &mut state.niri.casting.casts {
            if cast.stream_id != stream_id {
                continue;
            }
            cast.formats = FormatSet::default();
            let mut inner = cast.inner.borrow_mut();
            let size = inner.state.expected_format_size();
            let refresh = inner.refresh;
            let result = negotiation::request_shm_fallback(
                &cast.stream, &mut inner.state, size, cast.offer_alpha, refresh,
            );
            drop(inner);
            if let Err(err) = result {
                warn!("cannot recover capture with SHM: {err:#}");
                cast.stop_after_sync_failure();
            }
            break;
        }
    });
}
