use super::*;

impl Cast {
    fn queue_completed_buffers(&mut self) -> bool {
        let inner = self.inner.borrow();
        if inner.stopping {
            return false;
        }

        // We want to queue buffers in order, so find the first still-rendering buffer, and queue
        // everything up to that. Even if there are completed buffers past the first
        // still-rendering buffer, we do not want to queue them, since that would send frames out
        // of order.
        let first_in_progress_idx = inner
            .rendering_buffers
            .iter()
            .position(|(_, sync)| !sync.is_reached())
            .unwrap_or(inner.rendering_buffers.len());

        drop(inner);
        for _ in 0..first_in_progress_idx {
            let mut inner = self.inner.borrow_mut();
            if inner.stopping {
                return false;
            }
            // A PipeWire call may have retired a buffer or changed the pending queue.
            if !inner
                .rendering_buffers
                .first()
                .is_some_and(|(_, sync)| sync.is_reached())
            {
                break;
            }
            let (buffer, _) = inner.rendering_buffers.remove(0);
            // A previous fence can complete several frames before their own callbacks run.
            if let Some(token) = inner.fence_sources.remove(&buffer) {
                self.event_loop.remove(token);
            }
            // Never retain a RefCell borrow across a foreign call which may invoke callbacks.
            drop(inner);
            trace!("queueing completed buffer");
            unsafe {
                if let Err(err) = check_queue_result(pw_stream_queue_buffer(
                    self.stream.as_raw_ptr(),
                    buffer.as_ptr(),
                )) {
                    warn!("cannot deliver capture buffer: {err:#}");
                    self.stop_after_sync_failure();
                    return false;
                }
            }
        }
        true
    }

    pub(super) unsafe fn return_unused_buffer(&mut self, buffer: NonNull<pw_buffer>) {
        if let Err(err) = super::return_unused_buffer(&self.stream, buffer) {
            warn!("cannot return capture buffer: {err:#}");
            self.stop_after_sync_failure();
        }
    }

    pub(super) unsafe fn queue_after_sync(
        &mut self,
        pw_buffer: NonNull<pw_buffer>,
        sync_point: SyncPoint,
    ) -> bool {
        let _span = tracy_client::span!("Cast::queue_after_sync");

        let mut inner = self.inner.borrow_mut();
        if inner.stopping {
            return false;
        }

        // Original upstream rationale, retained for reference:
        // There are two main ways this can happen. First is that the SyncPoint is
        // pre-signalled, then the buffer is already ready and no waiting is needed. Second
        // is that the SyncPoint is potentially still not signalled, but exporting a fence
        // fd had failed. In this case, there's not much we can do (perhaps do a blocking
        // wait for the SyncPoint, which itself might fail).
        //
        // So let's hope for the best and mark the buffer as submittable. We do not reuse
        // the original SyncPoint because if we do hit the second case (when it's not
        // signalled), then without a sync fd we cannot schedule a queue upon its
        // completion, effectively going stuck. It's better to queue an incomplete buffer
        // than getting stuck.
        //
        // This fork instead stops capture if an unfinished fence cannot be exported;
        // it must not publish unfinished pixels or block the compositor thread.
        let sync_fd = export_pending_fence(&sync_point);
        inner.rendering_buffers.push((pw_buffer, sync_point));
        drop(inner);
        let sync_fd = match sync_fd {
            Ok(fd) => fd,
            Err(err) => {
                warn!("cannot synchronize capture frame: {err:#}");
                self.stop_after_sync_failure();
                return false;
            }
        };

        match sync_fd {
            None => {
                trace!("sync_fd is None, queueing completed buffers");
                // In case this is the only buffer in the list, we will queue it right away.
                return self.queue_completed_buffers();
            }
            Some(sync_fd) => {
                trace!("scheduling buffer to queue");
                let stream_id = self.stream_id;
                let source = Generic::new(sync_fd, Interest::READ, Mode::OneShot);
                let token = self.event_loop.insert_source(source, move |_, _, state| {
                    for cast in &mut state.niri.casting.casts {
                        if cast.stream_id == stream_id {
                            cast.inner.borrow_mut().fence_sources.remove(&pw_buffer);
                            cast.queue_completed_buffers();
                        }
                    }

                    Ok(PostAction::Remove)
                });
                match token {
                    Ok(token) => {
                        self.inner
                            .borrow_mut()
                            .fence_sources
                            .insert(pw_buffer, token);
                    }
                    Err(err) => {
                        warn!("cannot register capture fence: {err}");
                        self.stop_after_sync_failure();
                        return false;
                    }
                }
            }
        }
        true
    }

    pub(super) fn stop_after_sync_failure(&mut self) {
        let mut inner = self.inner.borrow_mut();
        if mem::replace(&mut inner.stopping, true) {
            return;
        }
        inner.is_active = false;
        inner.waiting_for_buffer = false;
        for (_, token) in inner.fence_sources.drain() {
            self.event_loop.remove(token);
        }
        inner.pending_shm.clear();
        drop(inner);
        self.remove_scheduled_redraw();
        let session_id = self.session_id;
        // Rendering temporarily moves casts out of State, so defer disconnection.
        self.event_loop
            .insert_idle(move |state| state.niri.stop_cast(session_id));
    }
}

pub(super) fn check_queue_result(result: i32) -> anyhow::Result<()> {
    if result < 0 {
        return Err(std::io::Error::from_raw_os_error(result.saturating_neg()).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_status_preserves_errno_and_accepts_nonnegative_success() {
        assert!(check_queue_result(0).is_ok());
        assert!(check_queue_result(1).is_ok());
        for code in [22, 32, 5] {
            let err = check_queue_result(-code).unwrap_err();
            assert_eq!(
                err.downcast_ref::<std::io::Error>().unwrap().raw_os_error(),
                Some(code)
            );
        }
    }
}
