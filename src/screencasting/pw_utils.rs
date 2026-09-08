use std::cell::RefCell;
use std::cmp::min;
use std::collections::HashMap;
use std::iter::zip;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;
use std::{mem, slice};

use anyhow::ensure;
use anyhow::Context as _;
use calloop::timer::{TimeoutAction, Timer};
use calloop::RegistrationToken;
use pipewire::context::ContextRc;
use pipewire::core::{CoreRc, PW_ID_CORE};
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::PropertiesBox;
use pipewire::spa::buffer::DataType;
use pipewire::spa::sys::*;
use pipewire::spa::utils::{Direction, Fraction};
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};
use pipewire::sys::{pw_buffer, pw_check_library_version, pw_stream_queue_buffer};
use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmBuffer, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::ExportMem;
use smithay::output::{Output, OutputModeSource};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::reexports::gbm::Modifier;
use smithay::utils::{Logical, Physical, Point, Scale, Size, Transform};
use zbus::object_server::SignalEmitter;

use crate::dbus::mutter_screen_cast::{self, CursorMode};
use crate::niri::{CastTarget, State};
use crate::render_helpers::{
    clear_dmabuf, encompassing_geo, render_and_download, render_to_dmabuf, StagingTexture,
};
use crate::screencasting::CastRenderElement;
use crate::utils::{get_monotonic_time, CastSessionId, CastStreamId};

// Give a 0.1 ms allowance for presentation time errors.
const CAST_DELAY_ALLOWANCE: Duration = Duration::from_micros(100);
const SHM_BLOCKS: usize = 1;
mod dmabuf_layout;
mod formats;
use formats::make_video_params_for_initial_negotiation_macro;
mod modifier_selection;
mod negotiation;
mod shm_buffer;
mod shm_mapping;
use shm_buffer::{
    allocate_shmbuf, clear_shmbuf, mark_shm_chunk_rendered, render_to_shmbuf, ShmLayout, Shmbuf,
};

const CURSOR_FORMAT: spa_video_format = SPA_VIDEO_FORMAT_BGRA;
const CURSOR_BPP: u32 = 4;
const CURSOR_WIDTH: u32 = 384;
const CURSOR_HEIGHT: u32 = 384;
const CURSOR_BITMAP_SIZE: usize = (CURSOR_WIDTH * CURSOR_HEIGHT * CURSOR_BPP) as usize;
const CURSOR_META_SIZE: usize =
    mem::size_of::<spa_meta_cursor>() + mem::size_of::<spa_meta_bitmap>() + CURSOR_BITMAP_SIZE;
const BITMAP_META_OFFSET: usize = mem::size_of::<spa_meta_cursor>();
const BITMAP_DATA_OFFSET: usize = mem::size_of::<spa_meta_bitmap>();

pub struct PipeWire {
    _context: ContextRc,
    pub core: CoreRc,
    pub token: RegistrationToken,
    event_loop: LoopHandle<'static, State>,
    to_niri: calloop::channel::Sender<PwToNiri>,
}

pub enum PwToNiri {
    StopCast { session_id: CastSessionId },
    Redraw { stream_id: CastStreamId },
    FatalError,
}

pub struct Cast {
    event_loop: LoopHandle<'static, State>,
    pub session_id: CastSessionId,
    pub stream_id: CastStreamId,
    // Listener is dropped before Stream to prevent a use-after-free.
    _listener: StreamListener<()>,
    pub stream: StreamRc,
    pub target: CastTarget,
    pub dynamic_target: bool,
    formats: FormatSet,
    offer_alpha: bool,
    cursor_mode: CursorMode,
    pub last_frame_time: Duration,
    scheduled_redraw: Option<RegistrationToken>,
    shm_staging: StagingTexture,
    // Incremented once per successful frame, stored in buffer meta.
    sequence_counter: u64,
    inner: Rc<RefCell<CastInner>>,
}

/// Mutable `Cast` state shared with PipeWire callbacks.
#[derive(Debug)]
struct CastInner {
    is_active: bool,
    waiting_for_buffer: bool,
    node_id: Option<u32>,
    state: CastState,
    refresh: u32,
    min_time_between_frames: Duration,
    dmabufs: HashMap<i64, Dmabuf>,
    shmbufs: HashMap<i64, Shmbuf>,
    /// Buffers dequeued from PipeWire in process of rendering.
    ///
    /// This is an ordered list of buffers that we started rendering to and waiting for the
    /// rendering to complete. The completion can be checked from the `SyncPoint`s. The buffers are
    /// stored in order from oldest to newest, and the same ordering should be preserved when
    /// submitting completed buffers to PipeWire.
    rendering_buffers: Vec<(NonNull<pw_buffer>, SyncPoint)>,
    fence_sources: HashMap<NonNull<pw_buffer>, RegistrationToken>,
}

#[derive(Debug, Clone, Copy)]
struct DmaNegotiationResult {
    modifier: Modifier,
    plane_count: i32,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum CastState {
    // extra_negotiation_result = Some(_) means DMA sharing
    // extra_negotiation_result = None    means SHM sharing
    ResizePending {
        pending_size: Size<u32, Physical>,
    },
    ConfirmationPending {
        size: Size<u32, Physical>,
        alpha: bool,
        extra_negotiation_result: Option<DmaNegotiationResult>,
    },
    Ready {
        size: Size<u32, Physical>,
        alpha: bool,
        extra_negotiation_result: Option<DmaNegotiationResult>,
        // Lazily-initialized to keep the initialization to a single place.
        damage_tracker: Option<OutputDamageTracker>,
        cursor_damage_tracker: Option<OutputDamageTracker>,
        last_cursor_location: Option<Point<i32, Physical>>,
    },
}

#[derive(PartialEq, Eq)]
pub enum CastSizeChange {
    Ready,
    Pending,
}

/// Data for drawing a cursor either as metadata or embedded.
///
/// The cursor elements are expected to be at the start of the main elements slice. `elem_count` is
/// the count of the pointer elements. This way, the full slice includes both main and cursor
/// elements for embedded mode, and `&elements[elem_count..]` gives just the main elements for
/// metadata mode.
///
/// We have weird borrowed references here in order to support both metadata and embedded cases.
/// The cursor damage tracker needs a slice of impl Element at (0, 0), so we pass it `relocated`
/// (luckily, &impl Element also impls Element). Then, if we need to embed the cursor, we use the
/// full elements slice which starts with non-relocated pointer elements (that we borrow from).
#[derive(Debug)]
pub struct CursorData<'a, E> {
    /// Count of the pointer elements in the slice (index of the first non-pointer element).
    elem_count: usize,
    /// Cursor elements relocated to (0, 0).
    relocated: Vec<RelocateRenderElement<&'a E>>,
    /// Location of the cursor's hotspot in the video buffer.
    location: Point<i32, Physical>,
    /// Location of the cursor's hotspot on the cursor bitmap.
    hotspot: Point<i32, Physical>,
    /// Size of the elements' encompassing geo.
    size: Size<i32, Physical>,
    /// Scale the elements should be rendered at.
    scale: Scale<f64>,
}

impl<'a, E: Element> CursorData<'a, E> {
    pub fn compute(
        elements: &'a [E],
        elem_count: usize,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
    ) -> Self {
        let pointer_elements = &elements[..elem_count];
        let location = location.to_physical_precise_round(scale);

        let geo = encompassing_geo(scale, pointer_elements.iter());
        let relocated = Vec::from_iter(pointer_elements.iter().map(|elem| {
            RelocateRenderElement::from_element(elem, geo.loc.upscale(-1), Relocate::Relative)
        }));

        Self {
            elem_count,
            relocated,
            location,
            hotspot: location - geo.loc,
            size: geo.size,
            scale,
        }
    }
}

impl PipeWire {
    pub fn new(
        event_loop: LoopHandle<'static, State>,
        to_niri: calloop::channel::Sender<PwToNiri>,
    ) -> anyhow::Result<Self> {
        let main_loop = MainLoopRc::new(None).context("error creating MainLoop")?;
        let context = ContextRc::new(&main_loop, None).context("error creating Context")?;
        let core = context.connect_rc(None).context("error creating Core")?;

        let to_niri_ = to_niri.clone();
        let listener = core
            .add_listener_local()
            .error(move |id, seq, res, message| {
                warn!(id, seq, res, message, "pw error");

                // Reset PipeWire on connection errors.
                if id == PW_ID_CORE && res == -32 {
                    if let Err(err) = to_niri_.send(PwToNiri::FatalError) {
                        warn!("error sending FatalError to niri: {err:?}");
                    }
                }
            })
            .register();
        mem::forget(listener);

        struct AsFdWrapper(MainLoopRc);
        impl AsFd for AsFdWrapper {
            fn as_fd(&self) -> BorrowedFd<'_> {
                self.0.loop_().fd()
            }
        }
        let generic = Generic::new(AsFdWrapper(main_loop), Interest::READ, Mode::Level);
        let token = event_loop
            .insert_source(generic, move |_, wrapper, _| {
                let _span = tracy_client::span!("pipewire iteration");
                wrapper.0.loop_().iterate(Duration::ZERO);
                Ok(PostAction::Continue)
            })
            .unwrap();

        Ok(Self {
            _context: context,
            core,
            token,
            event_loop,
            to_niri,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_cast(
        &self,
        gbm: GbmDevice<DrmDeviceFd>,
        formats: FormatSet,
        session_id: CastSessionId,
        stream_id: CastStreamId,
        target: CastTarget,
        size: Size<i32, Physical>,
        refresh: u32,
        alpha: bool,
        mut cursor_mode: CursorMode,
        signal_ctx: SignalEmitter<'static>,
    ) -> anyhow::Result<Cast> {
        let _span = tracy_client::span!("PipeWire::start_cast");

        let to_niri_ = self.to_niri.clone();
        let stop_cast = move || {
            if let Err(err) = to_niri_.send(PwToNiri::StopCast { session_id }) {
                warn!(%session_id, "error sending StopCast to niri: {err:?}");
            }
        };
        let to_niri_ = self.to_niri.clone();
        let redraw = move || {
            if let Err(err) = to_niri_.send(PwToNiri::Redraw { stream_id }) {
                warn!(%stream_id, "error sending Redraw to niri: {err:?}");
            }
        };
        let redraw_ = redraw.clone();

        let stream = StreamRc::new(
            self.core.clone(),
            "niri-screen-cast-src",
            PropertiesBox::new(),
        )
        .context("error creating Stream")?;

        if cursor_mode == CursorMode::Metadata && !pw_version_supports_cursor_metadata() {
            debug!(
                "metadata cursor mode requested, but PipeWire is too old (need >= 1.4.8); \
                 switching to embedded cursor"
            );
            cursor_mode = CursorMode::Embedded;
        }

        let pending_size = Size::from((size.w as u32, size.h as u32));

        // Like in good old wayland-rs times...
        let inner = Rc::new(RefCell::new(CastInner {
            is_active: false,
            waiting_for_buffer: false,
            node_id: None,
            state: CastState::ResizePending { pending_size },
            refresh,
            min_time_between_frames: negotiated_frame_interval(Fraction {
                num: refresh,
                denom: 1000,
            })
            .unwrap_or(Duration::ZERO),
            dmabufs: HashMap::new(),
            shmbufs: HashMap::new(),
            rendering_buffers: Vec::new(),
            fence_sources: HashMap::new(),
        }));

        let listener = stream
            .add_local_listener_with_user_data(())
            .process({
                let inner = inner.clone();
                let redraw = redraw.clone();
                move |_, ()| {
                    let mut inner = inner.borrow_mut();
                    if inner.is_active && mem::take(&mut inner.waiting_for_buffer) {
                        drop(inner);
                        redraw();
                    }
                }
            })
            .state_changed({
                let inner = inner.clone();
                let stop_cast = stop_cast.clone();
                move |stream, (), old, new| {
                    let _span = debug_span!("state_changed", %stream_id).entered();
                    debug!("{old:?} -> {new:?}");
                    let mut inner = inner.borrow_mut();

                    match new {
                        StreamState::Paused => {
                            if inner.node_id.is_none() {
                                let id = stream.node_id();
                                inner.node_id = Some(id);
                                debug!("sending signal with {id}");

                                let _span = tracy_client::span!("sending PipeWireStreamAdded");
                                async_io::block_on(async {
                                    let res = mutter_screen_cast::Stream::pipe_wire_stream_added(
                                        &signal_ctx,
                                        id,
                                    )
                                    .await;

                                    if let Err(err) = res {
                                        warn!("error sending PipeWireStreamAdded: {err:?}");
                                        stop_cast();
                                    }
                                });
                            }

                            inner.is_active = false;
                            inner.waiting_for_buffer = false;
                        }
                        StreamState::Error(_) => {
                            if inner.is_active {
                                inner.is_active = false;
                                stop_cast();
                            }
                        }
                        StreamState::Unconnected => (),
                        StreamState::Connecting => (),
                        StreamState::Streaming => {
                            inner.is_active = true;
                            inner.state.invalidate_damage();
                            redraw();
                        }
                    }
                }
            })
            .param_changed(negotiation::listener(
                inner.clone(), stop_cast.clone(), gbm.clone(), formats.clone(),
                stream_id, cursor_mode,
            ))
            .add_buffer({
                let inner = inner.clone();
                let stop_cast = stop_cast.clone();
                move |stream, (), buffer| {
                    let _span = debug_span!("add_buffer", %stream_id).entered();
                    let mut inner = inner.borrow_mut();

                    match inner.state {
                        CastState::Ready {
                            size,
                            alpha,
                            extra_negotiation_result,
                            ..
                        } => {
                            match extra_negotiation_result {
                                Some(DmaNegotiationResult { modifier, .. }) => {
                                    trace!("pw stream: add_buffer (dma), size={size:?}, alpha={alpha}, modifier={modifier:?}");
                                    unsafe {
                                        let spa_buffer = (*buffer).buffer;

                                        let fourcc = if alpha {
                                            Fourcc::Argb8888
                                        } else {
                                            Fourcc::Xrgb8888
                                        };

                                        let dmabuf = match allocate_dmabuf(&gbm, size, fourcc, modifier) {
                                            Ok(dmabuf) => dmabuf,
                                            Err(err) => {
                                                warn!("error allocating dmabuf: {err:?}");
                                                stop_cast();
                                                return;
                                            }
                                        };

                                        let plane_count = dmabuf.num_planes();
                                        assert_eq!((*spa_buffer).n_datas as usize, plane_count);
                                        let plane_sizes = match dmabuf_layout::plane_sizes(&dmabuf) {
                                            Ok(sizes) => sizes,
                                            Err(err) => {
                                                warn!("invalid DMA-BUF plane layout: {err:?}");
                                                stop_cast();
                                                return;
                                            }
                                        };

                                        for (i, (fd, (stride, offset))) in
                                            zip(dmabuf.handles(), zip(dmabuf.strides(), dmabuf.offsets()))
                                                .enumerate()
                                        {
                                            let spa_data = (*spa_buffer).datas.add(i);
                                            assert!((*spa_data).type_ & (1 << DataType::DmaBuf.as_raw()) > 0);

                                            (*spa_data).type_ = DataType::DmaBuf.as_raw();

                                            // With DMA-BUFs, consumers should ignore the maxsize field, and
                                            // producers are allowed to set it to 0.
                                            //
                                            // https://docs.pipewire.org/page_dma_buf.html
                                            //
                                            // GStreamer also uses this extent to locate linear video
                                            // planes. Publish the allocation size, not a one-byte sentinel.
                                            (*spa_data).maxsize = plane_sizes[i];
                                            (*spa_data).fd = fd.as_raw_fd() as i64;
                                            (*spa_data).flags = SPA_DATA_FLAG_READWRITE;

                                            let chunk = (*spa_data).chunk;
                                            (*chunk).stride = stride as i32;
                                            (*chunk).offset = offset;

                                            trace!(
                                                "pw buffer plane: fd={}, stride={stride}, offset={offset}",
                                                (*spa_data).fd
                                            );
                                        }

                                        let fd = (*(*spa_buffer).datas).fd;
                                        assert!(inner.dmabufs.insert(fd, dmabuf).is_none());
                                    }

                                    // During size re-negotiation, the stream sometimes just keeps running, in
                                    // which case we may need to force a redraw once we got a newly sized buffer.
                                    if inner.dmabufs.len() == 1 {
                                        inner.state.invalidate_damage();
                                        if stream.state() == StreamState::Streaming {
                                            redraw_();
                                        }
                                    }
                                },
                                None => {
                                    trace!("pw stream: add_buffer (shm), size={size:?}, alpha={alpha}");
                                    unsafe {
                                        let spa_buffer = (*buffer).buffer;

                                        let shmbuf = match allocate_shmbuf(size) {
                                            Ok(x) => x,
                                            Err(err) => {
                                                warn!("error allocating shmbuf: {err:?}");
                                                stop_cast();
                                                return;
                                            }
                                        };

                                        assert_eq!((*spa_buffer).n_datas as usize, SHM_BLOCKS);

                                        let spa_data = (*spa_buffer).datas;
                                        assert!((*spa_data).type_ & (1 << DataType::MemFd.as_raw()) > 0);

                                        (*spa_data).type_ = DataType::MemFd.as_raw();
                                        (*spa_data).maxsize = shmbuf.layout.size;
                                        (*spa_data).fd = shmbuf.fd.as_raw_fd() as i64;
                                        (*spa_data).flags = SPA_DATA_FLAG_READWRITE;

                                        let fd = (*(*spa_buffer).datas).fd;
                                        assert!(inner.shmbufs.insert(fd, shmbuf).is_none());
                                    }
                                    // A resize may leave the stream running without a state change.
                                    if inner.shmbufs.len() == 1 {
                                        inner.state.invalidate_damage();
                                        if stream.state() == StreamState::Streaming {
                                            redraw_();
                                        }
                                    }
                                }
                            }
                        },
                        _ => {
                            trace!("pw stream: add buffer, but not ready yet");
                        }
                    }
                }
            })
            .remove_buffer({
                let inner = inner.clone();
                let event_loop = self.event_loop.clone();
                move |_stream, (), buffer| {
                    trace!(%stream_id, "remove_buffer");
                    let mut inner = inner.borrow_mut();
                    if let Some(buffer) = NonNull::new(buffer) {
                        if let Some(token) = inner.fence_sources.remove(&buffer) {
                            event_loop.remove(token);
                        }
                    }

                    inner
                        .rendering_buffers
                        .retain(|(buf, _)| buf.as_ptr() != buffer);

                    unsafe {
                        let spa_buffer = (*buffer).buffer;
                        let spa_data = (*spa_buffer).datas;

                        if (*spa_data).type_ == DataType::DmaBuf.as_raw() {
                            trace!("pw stream: remove_buffer (dma)");
                            assert!((*spa_buffer).n_datas > 0);

                            let fd = (*spa_data).fd;
                            inner.dmabufs.remove(&fd);
                        } else if (*spa_data).type_ == DataType::MemFd.as_raw() {
                            trace!("pw stream: remove_buffer (shm)");
                            assert_eq!((*spa_buffer).n_datas, SHM_BLOCKS as u32);
                            let fd = (*spa_data).fd;
                            inner.shmbufs.remove(&fd);
                        } else {
                            warn!("pw stream: remove_buffer (unknown), impossible case happens, {:?}", (*spa_data).type_);
                        }
                    }
                }
            })
            .register()
            .unwrap();

        trace!(
            %stream_id,
            "starting pw stream with size={pending_size:?}, refresh={refresh:?}"
        );

        make_video_params_for_initial_negotiation_macro!(
            params,
            &formats,
            pending_size,
            refresh,
            alpha
        );
        stream
            .connect(
                Direction::Output,
                None,
                StreamFlags::DRIVER | StreamFlags::ALLOC_BUFFERS,
                params.clone().as_mut_slice(),
            )
            .context("error connecting stream")?;

        let cast = Cast {
            event_loop: self.event_loop.clone(),
            session_id,
            stream_id,
            stream,
            _listener: listener,
            target,
            dynamic_target: false,
            formats,
            offer_alpha: alpha,
            cursor_mode,
            last_frame_time: Duration::ZERO,
            scheduled_redraw: None,
            shm_staging: StagingTexture::default(),
            sequence_counter: 0,
            inner,
        };
        Ok(cast)
    }
}

impl Cast {
    pub fn is_active(&self) -> bool {
        self.inner.borrow().is_active
    }

    pub fn node_id(&self) -> Option<u32> {
        self.inner.borrow().node_id
    }

    pub fn ensure_size(&self, size: Size<i32, Physical>) -> anyhow::Result<CastSizeChange> {
        let mut inner = self.inner.borrow_mut();

        let new_size = Size::from((size.w as u32, size.h as u32));

        let state = &mut inner.state;
        if matches!(state, CastState::Ready { size, .. } if *size == new_size) {
            return Ok(CastSizeChange::Ready);
        }

        if state.pending_size() == Some(new_size) {
            debug!("stream size still hasn't changed, skipping frame");
            return Ok(CastSizeChange::Pending);
        }

        let _span = tracy_client::span!("Cast::ensure_size");
        debug!("cast size changed, updating stream size");

        *state = CastState::ResizePending {
            pending_size: new_size,
        };

        make_video_params_for_initial_negotiation_macro!(
            params,
            &self.formats,
            new_size,
            inner.refresh,
            self.offer_alpha
        );
        self.stream
            .update_params(params.clone().as_mut_slice())
            .context("error updating stream params")?;

        Ok(CastSizeChange::Pending)
    }

    pub fn set_refresh(&mut self, refresh: u32) -> anyhow::Result<()> {
        let mut inner = self.inner.borrow_mut();

        if inner.refresh == refresh {
            return Ok(());
        }

        let _span = tracy_client::span!("Cast::set_refresh");
        debug!("cast FPS changed, updating stream FPS");
        inner.refresh = refresh;

        let size = inner.state.expected_format_size();
        make_video_params_for_initial_negotiation_macro!(
            params,
            &self.formats,
            size,
            refresh,
            self.offer_alpha
        );
        self.stream
            .update_params(params.clone().as_mut_slice())
            .context("error updating stream params")?;

        Ok(())
    }

    fn compute_extra_delay(&self, target_frame_time: Duration) -> Duration {
        let inner = self.inner.borrow();

        let last = self.last_frame_time;
        let min = inner.min_time_between_frames;

        if last.is_zero() {
            trace!(?target_frame_time, ?last, "last is zero, recording");
            return Duration::ZERO;
        }

        if target_frame_time < last {
            // Record frame with a warning; in case it was an overflow this will fix it.
            warn!(
                ?target_frame_time,
                ?last,
                "target frame time is below last, did it overflow or did we mispredict?"
            );
            return Duration::ZERO;
        }

        let diff = target_frame_time - last;
        if diff < min {
            let delay = min - diff;
            trace!(
                ?target_frame_time,
                ?last,
                "frame is too soon: min={min:?}, delay={:?}",
                delay
            );
            return delay;
        } else {
            trace!("overshoot={:?}", diff - min);
        }

        Duration::ZERO
    }

    fn schedule_redraw(&mut self, output: Output, target_time: Duration) {
        if self.scheduled_redraw.is_some() {
            return;
        }

        let now = get_monotonic_time();
        let duration = target_time.saturating_sub(now);
        let timer = Timer::from_duration(duration);
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                // Guard against output disconnecting before the timer has a chance to run.
                if state.niri.output_state.contains_key(&output) {
                    state.niri.queue_redraw(&output);
                }

                TimeoutAction::Drop
            })
            .unwrap();
        self.scheduled_redraw = Some(token);
    }

    fn remove_scheduled_redraw(&mut self) {
        if let Some(token) = self.scheduled_redraw.take() {
            self.event_loop.remove(token);
        }
    }

    /// Checks whether this frame should be skipped because it's too soon.
    ///
    /// If the frame should be skipped, schedules a redraw and returns `true`. Otherwise, removes a
    /// scheduled redraw, if any, and returns `false`.
    ///
    /// When this method returns `false`, the calling code is assumed to follow up with
    /// [`Cast::dequeue_buffer_and_render()`].
    pub fn check_time_and_schedule(
        &mut self,
        output: &Output,
        target_frame_time: Duration,
    ) -> bool {
        let delay = self.compute_extra_delay(target_frame_time);
        if delay >= CAST_DELAY_ALLOWANCE {
            trace!("delay >= allowance, scheduling redraw");
            self.schedule_redraw(output.clone(), target_frame_time + delay);
            true
        } else {
            self.remove_scheduled_redraw();
            false
        }
    }

    fn dequeue_available_buffer(&mut self) -> Option<NonNull<pw_buffer>> {
        unsafe { NonNull::new(self.stream.dequeue_raw_buffer()) }
    }

    fn queue_completed_buffers(&mut self) {
        let mut inner = self.inner.borrow_mut();

        // We want to queue buffers in order, so find the first still-rendering buffer, and queue
        // everything up to that. Even if there are completed buffers past the first
        // still-rendering buffer, we do not want to queue them, since that would send frames out
        // of order.
        let first_in_progress_idx = inner
            .rendering_buffers
            .iter()
            .position(|(_, sync)| !sync.is_reached())
            .unwrap_or(inner.rendering_buffers.len());

        let CastInner { rendering_buffers, fence_sources, .. } = &mut *inner;
        for (buffer, _) in rendering_buffers.drain(..first_in_progress_idx) {
            // A previous fence can complete several frames before their own callbacks run.
            if let Some(token) = fence_sources.remove(&buffer) {
                self.event_loop.remove(token);
            }
            trace!("queueing completed buffer");
            unsafe {
                pw_stream_queue_buffer(self.stream.as_raw_ptr(), buffer.as_ptr());
            }
        }
    }

    unsafe fn queue_after_sync(&mut self, pw_buffer: NonNull<pw_buffer>, sync_point: SyncPoint) -> bool {
        let _span = tracy_client::span!("Cast::queue_after_sync");

        let mut inner = self.inner.borrow_mut();

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
                self.queue_completed_buffers();
            }
            Some(sync_fd) => {
                trace!("scheduling buffer to queue");
                let stream_id = self.stream_id;
                let source = Generic::new(sync_fd, Interest::READ, Mode::OneShot);
                let token = self.event_loop
                    .insert_source(source, move |_, _, state| {
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
                        self.inner.borrow_mut().fence_sources.insert(pw_buffer, token);
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

    fn stop_after_sync_failure(&mut self) {
        self.inner.borrow_mut().is_active = false;
        let session_id = self.session_id;
        // Rendering temporarily moves casts out of State, so defer disconnection.
        self.event_loop.insert_idle(move |state| state.niri.stop_cast(session_id));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn dequeue_buffer_and_render(
        &mut self,
        renderer: &mut GlesRenderer,
        mut elements: &[CastRenderElement<GlesRenderer>],
        cursor_data: &CursorData<CastRenderElement<GlesRenderer>>,
        size: Size<i32, Physical>,
        scale: Scale<f64>,
    ) -> bool {
        let mut inner = self.inner.borrow_mut();

        if let CastState::Ready {
            damage_tracker,
            cursor_damage_tracker,
            last_cursor_location,
            ..
        } = &mut inner.state
        {
            let damage_tracker = damage_tracker
                .get_or_insert_with(|| OutputDamageTracker::new(size, scale, Transform::Normal));
            let cursor_damage_tracker = cursor_damage_tracker.get_or_insert_with(|| {
                OutputDamageTracker::new(
                    Size::from((CURSOR_WIDTH as _, CURSOR_HEIGHT as _)),
                    scale,
                    Transform::Normal,
                )
            });

            // Size change will drop the damage tracker, but scale change won't, so check it here.
            let OutputModeSource::Static { scale: t_scale, .. } = damage_tracker.mode() else {
                unreachable!();
            };
            if *t_scale != scale {
                *damage_tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
                *cursor_damage_tracker = OutputDamageTracker::new(
                    Size::from((CURSOR_WIDTH as _, CURSOR_HEIGHT as _)),
                    scale,
                    Transform::Normal,
                );
            }

            let mut has_cursor_update = false;
            let mut redraw_cursor = false;

            // For embedded cursor, pass the full slice (cursor + main) to the damage tracker.
            // For metadata or hidden cursor, pass only the main elements.
            if self.cursor_mode == CursorMode::Metadata || self.cursor_mode == CursorMode::Hidden {
                elements = &elements[cursor_data.elem_count..];
            }
            let (damage, states) = damage_tracker.damage_output(1, elements).unwrap();

            if self.cursor_mode == CursorMode::Metadata {
                let (damage, _states) = cursor_damage_tracker
                    .damage_output(1, &cursor_data.relocated)
                    .unwrap();
                redraw_cursor = damage.is_some();
                has_cursor_update =
                    redraw_cursor || *last_cursor_location != Some(cursor_data.location);
            }

            if damage.is_none() && !has_cursor_update {
                trace!("no damage, skipping frame");
                return false;
            }
            *last_cursor_location = Some(cursor_data.location);
            drop(inner);

            let Some(pw_buffer) = self.dequeue_available_buffer() else {
                warn!("no available buffer in pw stream, skipping frame");
                let mut inner = self.inner.borrow_mut();
                inner.state.invalidate_damage();
                inner.waiting_for_buffer = true;
                return false;
            };
            self.inner.borrow_mut().waiting_for_buffer = false;
            let buffer = pw_buffer.as_ptr();

            let mut inner = self.inner.borrow_mut();
            let inner_ = &mut *inner;
            let CastState::Ready {
                damage_tracker,
                extra_negotiation_result,
                alpha,
                ..
            } = &mut inner_.state
            else {
                unreachable!()
            };
            let damage_tracker = damage_tracker.as_mut().unwrap();
            let extra_negotiation_result = extra_negotiation_result.clone();
            let alpha = *alpha;

            unsafe {
                let spa_buffer = (*buffer).buffer;

                if self.cursor_mode == CursorMode::Metadata {
                    add_cursor_metadata(renderer, spa_buffer, cursor_data, redraw_cursor);
                }

                // FIXME: would be good to skip rendering the full frame if only the pointer changed.
                // Unfortunately, I think the OBS PipeWire code needs to be updated first to cleanly
                // allow for that codepath.
                let fd = (*(*spa_buffer).datas).fd;

                match extra_negotiation_result {
                    Some(_) => {
                        let dmabuf = inner_.dmabufs[&fd].clone();
                        let res =
                            render_to_dmabuf(renderer, damage_tracker, dmabuf, elements, states);
                        drop(inner);

                        match res {
                            Ok(sync_point) => {
                                mark_buffer_after_render(
                                    pw_buffer,
                                    &mut self.sequence_counter,
                                    SharingBuf::DMA(()),
                                );
                                trace!("queueing buffer with seq={}", self.sequence_counter);
                                self.queue_after_sync(pw_buffer, sync_point)
                            }
                            Err(err) => {
                                warn!("error rendering to dmabuf: {err:?}");
                                self.inner.borrow_mut().state.invalidate_damage();
                                return_unused_buffer(&self.stream, pw_buffer);
                                false
                            }
                        }
                    }
                    None => {
                        let shmbuf = inner_.shmbufs[&fd].clone();
                        drop(inner);

                        let fourcc = if alpha {
                            Fourcc::Argb8888
                        } else {
                            Fourcc::Xrgb8888
                        };

                        match render_to_shmbuf(
                            renderer,
                            &mut self.shm_staging,
                            &shmbuf,
                            size,
                            scale,
                            Transform::Normal,
                            fourcc,
                            elements,
                        ) {
                            Ok(()) => {
                                mark_buffer_after_render(
                                    pw_buffer,
                                    &mut self.sequence_counter,
                                    SharingBuf::SHM(&shmbuf),
                                );
                                trace!("queueing buffer with seq={}", self.sequence_counter);
                                self.queue_after_sync(pw_buffer, SyncPoint::signaled())
                            }
                            Err(err) => {
                                warn!("error rendering to shmbuf: {err:?}");
                                self.inner.borrow_mut().state.invalidate_damage();
                                return_unused_buffer(&self.stream, pw_buffer);
                                false
                            }
                        }
                    }
                }
            }
        } else {
            error!("cast must be in Ready state to render");
            false
        }
    }

    pub fn dequeue_buffer_and_clear(&mut self, renderer: &mut GlesRenderer) -> bool {
        let mut inner = self.inner.borrow_mut();

        // Clear out the damage tracker if we're in Ready state.
        if let CastState::Ready {
            damage_tracker,
            cursor_damage_tracker,
            ..
        } = &mut inner.state
        {
            *damage_tracker = None;
            *cursor_damage_tracker = None;
        };
        drop(inner);

        let Some(pw_buffer) = self.dequeue_available_buffer() else {
            warn!("no available buffer in pw stream, skipping frame");
            return false;
        };
        let buffer = pw_buffer.as_ptr();

        unsafe {
            if (*(*(*buffer).buffer).datas).type_ == DataType::DmaBuf.as_raw() {
                let spa_buffer = (*buffer).buffer;

                if self.cursor_mode == CursorMode::Metadata {
                    add_invisible_cursor(spa_buffer);
                }

                let fd = (*(*spa_buffer).datas).fd;
                let dmabuf = self.inner.borrow().dmabufs[&fd].clone();

                match clear_dmabuf(renderer, dmabuf) {
                    Ok(sync_point) => {
                        mark_buffer_after_render(
                            pw_buffer,
                            &mut self.sequence_counter,
                            SharingBuf::DMA(()),
                        );
                        trace!("queueing clear buffer with seq={}", self.sequence_counter);
                        self.queue_after_sync(pw_buffer, sync_point)
                    }
                    Err(err) => {
                        warn!("error clearing dmabuf: {err:?}");
                        return_unused_buffer(&self.stream, pw_buffer);
                        false
                    }
                }
            } else if (*(*(*buffer).buffer).datas).type_ == DataType::MemFd.as_raw() {
                let blocks = (*(*buffer).buffer).n_datas;

                if blocks as usize != SHM_BLOCKS {
                    warn!("expected {SHM_BLOCKS} blocks, got {blocks}");
                    return false;
                };

                let spa_buffer = (*buffer).buffer;

                if self.cursor_mode == CursorMode::Metadata {
                    add_invisible_cursor(spa_buffer);
                }

                let fd = (*(*spa_buffer).datas).fd;
                let shmbuf = self.inner.borrow().shmbufs[&fd].clone();

                match clear_shmbuf(&shmbuf) {
                    Ok(()) => {
                        mark_buffer_after_render(
                            pw_buffer,
                            &mut self.sequence_counter,
                            SharingBuf::SHM(&shmbuf),
                        );
                        trace!("queueing clear buffer with seq={}", self.sequence_counter);
                        self.queue_after_sync(pw_buffer, SyncPoint::signaled())
                    }
                    Err(err) => {
                        warn!("error clearing shmbuf: {err:?}");
                        return_unused_buffer(&self.stream, pw_buffer);
                        false
                    }
                }
            } else {
                warn!("unknown data type in dequeue_buffer_and_clear");
                false
            }
        }
    }
}

impl Drop for Cast {
    fn drop(&mut self) {
        self.remove_scheduled_redraw();
        for (_, token) in self.inner.borrow_mut().fence_sources.drain() {
            self.event_loop.remove(token);
        }
    }
}

impl CastState {
    /// A resumed consumer or a new buffer pool needs a frame even on a static scene.
    fn invalidate_damage(&mut self) {
        if let Self::Ready { damage_tracker, cursor_damage_tracker, last_cursor_location, .. } = self {
            *damage_tracker = None;
            *cursor_damage_tracker = None;
            *last_cursor_location = None;
        }
    }

    fn pending_size(&self) -> Option<Size<u32, Physical>> {
        match self {
            CastState::ResizePending { pending_size } => Some(*pending_size),
            CastState::ConfirmationPending { size, .. } => Some(*size),
            CastState::Ready { .. } => None,
        }
    }

    fn expected_format_size(&self) -> Size<u32, Physical> {
        match self {
            CastState::ResizePending { pending_size } => *pending_size,
            CastState::ConfirmationPending { size, .. } => *size,
            CastState::Ready { size, .. } => *size,
        }
    }
}

fn pw_version_supports_cursor_metadata() -> bool {
    // This PipeWire version fixed a critical memory issue with cursor metadata:
    // https://gitlab.freedesktop.org/pipewire/pipewire/-/merge_requests/2538
    unsafe { pw_check_library_version(1, 4, 8) }
}

fn negotiated_frame_interval(rate: Fraction) -> Option<Duration> {
    if rate.num == 0 || rate.denom == 0 {
        return None;
    }
    Some(Duration::from_micros(
        1_000_000 * u64::from(rate.denom) / u64::from(rate.num),
    ))
}

fn find_preferred_modifier(
    gbm: &GbmDevice<DrmDeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifiers: Vec<i64>,
) -> anyhow::Result<(Modifier, usize)> {
    debug!("find_preferred_modifier: size={size:?}, fourcc={fourcc}, modifiers={modifiers:?}");

    modifier_selection::try_modifiers(&modifiers, |offered| {
        let (buffer, modifier) = allocate_buffer(gbm, size, fourcc, offered)?;
        let dmabuf = buffer
            .export()
            .context("error exporting GBM buffer object as dmabuf")?;
        dmabuf_layout::plane_sizes(&dmabuf)?;
        let plane_count = dmabuf.num_planes();

        // FIXME: Ideally this also needs to try binding the dmabuf for rendering.

        Ok((modifier, plane_count))
    })
}

fn allocate_buffer(
    gbm: &GbmDevice<DrmDeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifiers: &[i64],
) -> anyhow::Result<(GbmBuffer, Modifier)> {
    let (w, h) = (size.w, size.h);
    let flags = GbmBufferFlags::RENDERING;

    if modifiers.len() == 1 && Modifier::from(modifiers[0] as u64) == Modifier::Invalid {
        let bo = gbm
            .create_buffer_object::<()>(w, h, fourcc, flags)
            .context("error creating GBM buffer object")?;

        let buffer = GbmBuffer::from_bo(bo, true);
        Ok((buffer, Modifier::Invalid))
    } else {
        let modifiers = modifiers
            .iter()
            .map(|m| Modifier::from(*m as u64))
            .filter(|m| *m != Modifier::Invalid);

        let bo = gbm
            .create_buffer_object_with_modifiers2::<()>(w, h, fourcc, modifiers, flags)
            .context("error creating GBM buffer object")?;

        let modifier = bo.modifier();
        let buffer = GbmBuffer::from_bo(bo, false);
        Ok((buffer, modifier))
    }
}

fn allocate_dmabuf(
    gbm: &GbmDevice<DrmDeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifier: Modifier,
) -> anyhow::Result<Dmabuf> {
    let (buffer, allocated_modifier) =
        allocate_buffer(gbm, size, fourcc, &[u64::from(modifier) as i64])?;
    ensure!(
        allocated_modifier == modifier,
        "allocated DMA-BUF modifier differs from negotiated modifier"
    );
    let dmabuf = buffer
        .export()
        .context("error exporting GBM buffer object as dmabuf")?;
    for stride in dmabuf.strides() {
        i32::try_from(stride).context("DMA-BUF stride exceeds SPA i32")?;
    }
    Ok(dmabuf)
}

enum SharingBuf<'a> {
    DMA(()),
    SHM(&'a Shmbuf),
}

fn export_pending_fence(sync: &SyncPoint) -> anyhow::Result<Option<std::os::fd::OwnedFd>> {
    if sync.is_reached() {
        return Ok(None);
    }
    if let Some(fd) = sync.export() {
        return Ok(Some(fd));
    }
    ensure!(sync.is_reached(), "unfinished GPU fence cannot be exported");
    Ok(None)
}

unsafe fn return_unused_buffer(stream: &Stream, pw_buffer: NonNull<pw_buffer>) {
    // pw_stream_return_buffer() requires too new PipeWire (1.4.0). So, mark as
    // corrupted and queue.
    let pw_buffer = pw_buffer.as_ptr();
    let spa_buffer = (*pw_buffer).buffer;
    // Some (older?) consumers will check for size == 0 instead of the CORRUPTED flag.
    for i in 0..(*spa_buffer).n_datas as usize {
        let chunk = (*(*spa_buffer).datas.add(i)).chunk;
        (*chunk).size = 0;
        (*chunk).flags = SPA_CHUNK_FLAG_CORRUPTED as i32;
    }

    if let Some(header) = find_meta_header(spa_buffer) {
        let header = header.as_ptr();
        (*header).flags = SPA_META_HEADER_FLAG_CORRUPTED;
    }

    pw_stream_queue_buffer(stream.as_raw_ptr(), pw_buffer);
}

unsafe fn mark_buffer_after_render(
    pw_buffer: NonNull<pw_buffer>,
    sequence: &mut u64,
    buf: SharingBuf,
) {
    let pw_buffer = pw_buffer.as_ptr();
    let spa_buffer = (*pw_buffer).buffer;
    let chunk = (*(*spa_buffer).datas).chunk;

    match buf {
        SharingBuf::DMA(_) => {
            // Original upstream sentinel policy, retained for reference:
            // With DMA-BUFs, consumers should ignore the size field, and producers are allowed
            // to set it to 0.
            //
            // https://docs.pipewire.org/page_dma_buf.html
            //
            // However, OBS checks for size != 0 as a workaround for old compositor versions,
            // so we set it to 1.
            //
            // This fork publishes the full extent instead of the upstream sentinel.
            // Restore the readable extent after returning an unused/corrupted buffer.
            // GStreamer needs the full extent even though PipeWire permits a sentinel.
            for i in 0..(*spa_buffer).n_datas as usize {
                let chunk = (*(*spa_buffer).datas.add(i)).chunk;
                (*chunk).size = (*(*spa_buffer).datas.add(i)).maxsize - (*chunk).offset;
                // Preserve each plane's stride and offset from allocation.
                (*chunk).flags = SPA_CHUNK_FLAG_NONE as i32;
            }
        }
        SharingBuf::SHM(shmbuf) => {
            mark_shm_chunk_rendered(&mut *chunk, shmbuf.layout);
        }
    }

    *sequence = sequence.wrapping_add(1);
    if let Some(header) = find_meta_header(spa_buffer) {
        let header = header.as_ptr();
        // Clear the corrupted flag we may have set before.
        (*header).flags = 0;
        (*header).seq = *sequence;
    }
}

unsafe fn find_meta_header(buffer: *mut spa_buffer) -> Option<NonNull<spa_meta_header>> {
    let p = spa_buffer_find_meta_data(buffer, SPA_META_Header, size_of::<spa_meta_header>()).cast();
    NonNull::new(p)
}

unsafe fn add_invisible_cursor(spa_buffer: *mut spa_buffer) {
    unsafe {
        let cursor_meta_ptr: *mut spa_meta_cursor = spa_buffer_find_meta_data(
            spa_buffer,
            SPA_META_Cursor,
            mem::size_of::<spa_meta_cursor>(),
        )
        .cast();
        let Some(cursor_meta) = cursor_meta_ptr.as_mut() else {
            return;
        };

        // The cursor is present but invisible.
        cursor_meta.id = 1;
        cursor_meta.position.x = 0;
        cursor_meta.position.y = 0;
        cursor_meta.hotspot.x = 0;
        cursor_meta.hotspot.y = 0;
        cursor_meta.bitmap_offset = BITMAP_META_OFFSET as _;

        let bitmap_meta_ptr = cursor_meta_ptr
            .byte_add(BITMAP_META_OFFSET)
            .cast::<spa_meta_bitmap>();
        let bitmap_meta = &mut *bitmap_meta_ptr;

        // HACK: PipeWire docs say offset = 0 means invisible.
        //
        // Unfortunately, OBS doesn't actually check that, instead it checks that size isn't zero:
        // https://github.com/obsproject/obs-studio/blob/f4aaa5f0417c5ec40a3799551e125129fce1e007/plugins/linux-pipewire/pipewire.c#L900
        //
        // Unfortunately, libwebrtc, on top of ignoring offset, also treats size = 0 as "preserve
        // previous cursor":
        // https://webrtc.googlesource.com/src/+/97b46e12582606a238d4f0c8524365cf5bdcb411/modules/desktop_capture/linux/wayland/shared_screencast_stream.cc#765
        //
        // So, send a 1x1 transparent pixel instead...
        bitmap_meta.offset = BITMAP_DATA_OFFSET as _;
        bitmap_meta.size.width = 1;
        bitmap_meta.size.height = 1;
        bitmap_meta.stride = CURSOR_BPP as i32;
        bitmap_meta.format = CURSOR_FORMAT;

        let bitmap_data = bitmap_meta_ptr.cast::<u8>().add(BITMAP_DATA_OFFSET);
        let bitmap_slice = slice::from_raw_parts_mut(bitmap_data, CURSOR_BITMAP_SIZE);
        bitmap_slice[..4].copy_from_slice(&[0, 0, 0, 0]);
    }
}

unsafe fn add_cursor_metadata(
    renderer: &mut GlesRenderer,
    spa_buffer: *mut spa_buffer,
    cursor_data: &CursorData<impl RenderElement<GlesRenderer>>,
    redraw: bool,
) {
    unsafe {
        let cursor_meta_ptr: *mut spa_meta_cursor = spa_buffer_find_meta_data(
            spa_buffer,
            SPA_META_Cursor,
            mem::size_of::<spa_meta_cursor>(),
        )
        .cast();
        let Some(cursor_meta) = cursor_meta_ptr.as_mut() else {
            return;
        };

        cursor_meta.id = 1;
        cursor_meta.position.x = cursor_data.location.x;
        cursor_meta.position.y = cursor_data.location.y;
        cursor_meta.hotspot.x = cursor_data.hotspot.x;
        cursor_meta.hotspot.y = cursor_data.hotspot.y;

        if !redraw {
            trace!("cursor not damaged, skipping rerendering");
            cursor_meta.bitmap_offset = 0;
            return;
        }

        cursor_meta.bitmap_offset = BITMAP_META_OFFSET as _;

        let bitmap_meta_ptr = cursor_meta_ptr
            .byte_add(BITMAP_META_OFFSET)
            .cast::<spa_meta_bitmap>();
        let bitmap_meta = &mut *bitmap_meta_ptr;

        // Start with a 1x1 transparent pixel; see comment in add_invisible_cursor().
        bitmap_meta.offset = BITMAP_DATA_OFFSET as _;
        bitmap_meta.size.width = 1;
        bitmap_meta.size.height = 1;
        bitmap_meta.stride = CURSOR_BPP as i32;
        bitmap_meta.format = CURSOR_FORMAT;

        let bitmap_data = bitmap_meta_ptr.cast::<u8>().add(BITMAP_DATA_OFFSET);
        let bitmap_slice = slice::from_raw_parts_mut(bitmap_data, CURSOR_BITMAP_SIZE);
        bitmap_slice[..4].copy_from_slice(&[0, 0, 0, 0]);

        let size = Size::new(
            min(cursor_data.size.w, CURSOR_WIDTH as i32),
            min(cursor_data.size.h, CURSOR_HEIGHT as i32),
        );
        if size.w == 0 || size.h == 0 {
            trace!("cursor is invisible, skipping rendering");
            return;
        }

        let _span = tracy_client::span!("add_cursor_metadata render cursor");

        // FIXME: use a reliable buffer whenever we're rendering the cursor.
        //
        // PipeWire buffers are not normally guaranteed to reach the destination, so our buffer
        // with the rendered cursor bitmap may not reach the consumer.
        //
        // Reliable buffers should be available starting from 1.6.0:
        // https://gitlab.freedesktop.org/pipewire/pipewire/-/issues/4885
        let mapping = match render_and_download(
            renderer,
            size,
            cursor_data.scale,
            Transform::Normal,
            Fourcc::Argb8888,
            cursor_data.relocated.iter().rev(),
        ) {
            Ok(mapping) => mapping,
            Err(err) => {
                warn!("error rendering cursor: {err:?}");
                return;
            }
        };
        let pixels = match renderer.map_texture(&mapping) {
            Ok(pixels) => pixels,
            Err(err) => {
                warn!("error mapping cursor texture: {err:?}");
                return;
            }
        };

        bitmap_slice[..pixels.len()].copy_from_slice(pixels);

        // Fill the metadata now that everything succeeded.
        bitmap_meta.size.width = size.w as _;
        bitmap_meta.size.height = size.h as _;
        bitmap_meta.stride = size.w * CURSOR_BPP as i32;
    }
}

#[cfg(test)]
mod tests;
