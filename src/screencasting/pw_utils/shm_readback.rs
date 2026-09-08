use super::*;

impl Cast {
    fn complete_shm_readback(
        &mut self,
        buffer: NonNull<pw_buffer>,
        renderer: &mut GlesRenderer,
    ) -> bool {
        let pending = self.inner.borrow_mut().pending_shm.remove(&buffer);
        let Some(pending) = pending else { return false };
        match pending.complete(renderer) {
            Ok(shmbuf) => unsafe {
                mark_buffer_after_render(
                    buffer,
                    &mut self.sequence_counter,
                    SharingBuf::SHM(&shmbuf),
                );
                self.queue_after_sync(buffer, SyncPoint::signaled())
            },
            Err(err) => {
                warn!("error completing SHM readback: {err:#}");
                self.inner.borrow_mut().state.invalidate_damage();
                unsafe {
                    self.return_unused_buffer(buffer);
                }
                self.stop_after_sync_failure();
                false
            }
        }
    }

    pub(super) fn submit_shm_readback(
        &mut self,
        pw_buffer: NonNull<pw_buffer>,
        readback: shm_buffer::ShmReadback,
        fence: Option<std::os::fd::OwnedFd>,
        renderer: &mut GlesRenderer,
    ) -> bool {
        self.inner
            .borrow_mut()
            .pending_shm
            .insert(pw_buffer, readback);
        if let Some(fd) = fence {
            let stream_id = self.stream_id;
            let token = self.event_loop.insert_source(
                Generic::new(fd, Interest::READ, Mode::OneShot),
                move |_, _, state| {
                    let mut redraw = false;
                    for cast in &mut state.niri.casting.casts {
                        if cast.stream_id == stream_id {
                            cast.inner.borrow_mut().fence_sources.remove(&pw_buffer);
                            let result = state.backend.with_primary_renderer(|renderer| {
                                cast.complete_shm_readback(pw_buffer, renderer)
                            });
                            if result != Some(true) {
                                cast.stop_after_sync_failure();
                            }
                            let mut inner = cast.inner.borrow_mut();
                            redraw = inner.is_active && mem::take(&mut inner.waiting_for_buffer);
                        }
                    }
                    if redraw {
                        state.redraw_cast(stream_id);
                    }
                    Ok(PostAction::Remove)
                },
            );
            match token {
                Ok(token) => {
                    self.inner
                        .borrow_mut()
                        .fence_sources
                        .insert(pw_buffer, token);
                }
                Err(err) => {
                    warn!("cannot register SHM readback fence: {err}");
                    self.stop_after_sync_failure();
                    return false;
                }
            }
        } else {
            return self.complete_shm_readback(pw_buffer, renderer);
        }
        true
    }
}
