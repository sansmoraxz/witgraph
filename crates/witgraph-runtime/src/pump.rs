//! Moving a stream from one Store to another.
//!
//! A component-model stream belongs to the Store it was made in. For
//! element types the host can lift and lower itself (the scalars and
//! `string`), a stream can still reach a reader in another Store: its
//! items are read out of the source Store into a bounded channel
//! ([`Sink`]), and a new stream in the destination Store yields what the
//! channel holds ([`Feed`]).
//!
//! The channel holds at most [`CAPACITY`] chunks, so the writer is held
//! back when the reader is slow, as it is inside one Store. A chunk takes
//! at most [`CHUNK_BYTES`] of items by their in-host size (`size_of`), so
//! a longer write completes short and the guest writes the rest again; the
//! contents of `string` items are not known before they are read, and are
//! bounded per read only by hostcall fuel, as for any value lifted out of
//! a guest. Every chunk, `string` contents included, is charged against
//! the island's `max_island_memory` while the pump holds it, and given back
//! when the reader's Store takes it (or the pump is dropped); a chunk over
//! the limit faults the island like a memory growth would. A read takes
//! as many items as the reader has room for, from as many chunks as the
//! channel holds (so a writer that writes one item at a time is read in
//! batches); the rest stay in the pump, still charged. A zero-length read
//! (a reader waiting to see items are ready) waits for a chunk, and keeps
//! it for the next read. When the reader
//! drops its end, the channel closes and the writer's next write reports
//! the drop; when the writer finishes, the reader sees the end of the
//! stream.
//!
//! A pump outlives the calls that made it: its source Store keeps running
//! until the pump has [ended](Ended) (the stream finished, or its reader let
//! go), because the stream's writer need not be a guest task. A node that
//! returns a stream it was given hands the host a stream the host itself
//! writes, which only that Store's event loop forwards. And a destination
//! Store [cuts](Intake::cut) the pumps it took in once its share of the
//! generation is done, so a writer upstream is never left waiting on a
//! reader that is gone.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::StreamExt;
use futures::channel::mpsc;
use futures::task::AtomicWaker;
use wasmtime::StoreContextMut;
use wasmtime::component::{
    Accessor, Destination, Lift, Lower, Source, StreamAny, StreamConsumer, StreamProducer,
    StreamReader, StreamResult, Type, Val, VecBuffer,
};
use witgraph_ir::PortName;
use witgraph_ir::wasm_wave::wasm::WasmTypeKind;
use witgraph_sched::{IslandEventKind, Reporter};

use crate::engine::{IslandData, IslandMemory};

/// Chunks a pump holds between its Stores before the writer waits.
const CAPACITY: usize = 4;

/// The most bytes of items (by `size_of`) one chunk takes.
const CHUNK_BYTES: usize = 64 * 1024;

/// The most items of type `T` one chunk takes.
const fn chunk_items<T>() -> usize {
    let size = std::mem::size_of::<T>();
    if size == 0 || size >= CHUNK_BYTES {
        1
    } else {
        CHUNK_BYTES / size
    }
}

/// Host memory an item holds besides its own `size_of`.
pub(crate) trait HeapSize {
    fn heap(&self) -> usize {
        0
    }
}

impl HeapSize for String {
    fn heap(&self) -> usize {
        self.capacity()
    }
}

macro_rules! no_heap {
    ($($t:ty),*) => { $(impl HeapSize for $t {})* };
}

no_heap!(bool, i8, u8, i16, u16, i32, u32, i64, u64, f32, f64, char);

/// The bytes charged for `items`: their own size, and what they hold on
/// the heap. Charging and releasing both count with this, so they agree.
fn chunk_bytes<T: HeapSize>(items: &[T]) -> usize {
    items.iter().fold(
        items.len().saturating_mul(std::mem::size_of::<T>()),
        |bytes, item| bytes.saturating_add(item.heap()),
    )
}

/// Chunks of a pump, each with the bytes charged for it.
type Chunk<T> = (Vec<T>, usize);

/// Whether a pump has ended: set when either end of it is dropped (the
/// source's [`Sink`] once the stream finished, the destination's [`Pipe`]
/// once the reader let go).
#[derive(Default)]
pub(crate) struct Ended {
    ended: AtomicBool,
    waker: AtomicWaker,
}

impl Ended {
    fn set(&self) {
        self.ended.store(true, Ordering::Release);
        self.waker.wake();
    }

    /// Ready once the pump has ended.
    pub(crate) fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.ended.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        self.waker.register(cx.waker());
        if self.ended.load(Ordering::Acquire) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// The receiving end of a pump's channel. Whatever the channel still holds
/// when it is dropped is given back to the island's memory.
pub(crate) struct Pipe<T> {
    rx: mpsc::Receiver<Chunk<T>>,
    memory: Arc<IslandMemory>,
    ended: Arc<Ended>,
}

impl<T> Drop for Pipe<T> {
    fn drop(&mut self) {
        self.rx.close();
        while let Ok((_, bytes)) = self.rx.try_recv() {
            self.memory.release(bytes);
        }
        self.ended.set();
    }
}

/// Reports the items passing through a pump as island events.
pub(crate) struct Tap {
    pub(crate) reporter: Reporter<Val>,
    /// The member whose output the stream is.
    pub(crate) member: usize,
    pub(crate) port: PortName,
}

impl Tap {
    fn passed(&self, count: usize) {
        self.reporter.send(IslandEventKind::StreamItems {
            member: self.member,
            port: self.port.clone(),
            count,
        });
    }
}

/// The source Store's end: forwards the stream's items into the channel,
/// waiting for room.
struct Sink<T> {
    tx: mpsc::Sender<Chunk<T>>,
    tap: Option<Tap>,
    memory: Arc<IslandMemory>,
    ended: Arc<Ended>,
}

impl<T> Drop for Sink<T> {
    fn drop(&mut self) {
        self.ended.set();
    }
}

impl<T, D> StreamConsumer<D> for Sink<T>
where
    T: Lift + HeapSize + Send + Sync + 'static,
    D: 'static,
{
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, T>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let this = self.get_mut();
        // A slot is reserved before anything is read, so nothing read is
        // ever left without room. A write the guest cancels while the
        // channel is full takes nothing.
        match this.tx.poll_ready(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(_)) => return Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending if finish => return Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => return Poll::Pending,
        }
        let wanted = source.remaining(&mut store).min(chunk_items::<T>());
        let mut items = Vec::with_capacity(wanted);
        source.read(&mut store, &mut items)?;
        if items.is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let count = items.len();
        let bytes = chunk_bytes(&items);
        this.memory.charge(bytes).map_err(wasmtime::Error::new)?;
        if this.tx.start_send((items, bytes)).is_err() {
            this.memory.release(bytes);
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        if let Some(tap) = &this.tap {
            tap.passed(count);
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// What a destination Store holds of a pump: the channel, and the items
/// taken out of it that no read has taken yet, with the bytes they are
/// charged.
struct Received<T> {
    pipe: Pipe<T>,
    pending: VecDeque<T>,
    held: usize,
}

impl<T> Received<T> {
    /// Moves a chunk out of the channel into `pending`, still charged.
    fn hold(&mut self, (items, bytes): Chunk<T>) {
        self.pending.extend(items);
        self.held = self.held.saturating_add(bytes);
    }
}

impl<T> Drop for Received<T> {
    fn drop(&mut self) {
        self.pipe.memory.release(self.held);
    }
}

/// A pump as its destination Store took it in, until it is cut.
type Slot<T> = Arc<Mutex<Option<Received<T>>>>;

/// Drops what `slot` holds, if anything.
fn cut<T>(slot: &Slot<T>) {
    let received = slot.lock().ok().and_then(|mut slot| slot.take());
    drop(received);
}

/// A pump a destination Store took in. Cutting it drops the pump, as if
/// the reader had dropped the stream: the writer's next write reports the
/// drop, and a reader still holding the stream sees it end.
pub(crate) struct Intake(Box<dyn FnOnce() + Send>);

impl Intake {
    fn of<T: Send + 'static>(slot: &Slot<T>) -> Self {
        let slot = slot.clone();
        Self(Box::new(move || cut(&slot)))
    }

    pub(crate) fn cut(self) {
        (self.0)();
    }
}

/// The destination Store's end: yields the chunks the channel holds.
struct Feed<T> {
    slot: Slot<T>,
}

impl<T> Drop for Feed<T> {
    fn drop(&mut self) {
        // The reader let go: the pump ends now, not when the Store cuts it.
        cut(&self.slot);
    }
}

impl<T, D> StreamProducer<D> for Feed<T>
where
    T: Lower + HeapSize + Send + Sync + 'static,
{
    type Item = T;
    type Buffer = VecBuffer<T>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        mut destination: Destination<'a, T, VecBuffer<T>>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let zero = destination.remaining(&mut store) == Some(0);
        let mut slot = self
            .slot
            .lock()
            .map_err(|_| wasmtime::format_err!("a pump's lock was poisoned"))?;
        // Cut: the stream has ended for this reader.
        let Some(this) = slot.as_mut() else {
            return Poll::Ready(Ok(StreamResult::Dropped));
        };
        if this.pending.is_empty() {
            match this.pipe.rx.poll_next_unpin(cx) {
                Poll::Ready(Some(chunk)) => this.hold(chunk),
                Poll::Ready(None) => return Poll::Ready(Ok(StreamResult::Dropped)),
                // A read the guest cancels while the channel is empty gets
                // nothing.
                Poll::Pending if finish => return Poll::Ready(Ok(StreamResult::Cancelled)),
                Poll::Pending => return Poll::Pending,
            }
        }
        // A zero-length read only asks whether items are ready: they stay
        // here, so nothing is lost if the reader never reads again.
        if zero {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        // A reader with room takes the chunks already waiting too.
        let room = destination.remaining(&mut store).unwrap_or(usize::MAX);
        while this.pending.len() < room {
            match this.pipe.rx.try_recv() {
                Ok(chunk) => this.hold(chunk),
                Err(_) => break,
            }
        }
        // Only what the reader takes now leaves the pump (and its charge):
        // the rest waits here, still counted, for the next read.
        let take = room.min(this.pending.len());
        let items: Vec<T> = this.pending.drain(..take).collect();
        let bytes = chunk_bytes(&items);
        this.held = this.held.saturating_sub(bytes);
        this.pipe.memory.release(bytes);
        destination.set_buffer(VecBuffer::from(items));
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// The element types a pump can carry, with what each needs: the receiving
/// end of a pump ([`StreamRx`]) and the two halves of the move
/// ([`export_stream`], [`import_stream`]). Loading puts two members in
/// separate Stores only when the streams between them carry one of these.
macro_rules! pumped {
    ($($variant:ident ($kind:ident) => $t:ty),* $(,)?) => {
        /// Whether a stream of `kind` items can be pumped between Stores.
        pub(crate) fn can_pump(kind: WasmTypeKind) -> bool {
            matches!(kind, $(WasmTypeKind::$kind)|*)
        }

        /// The receiving end of a pump, by element type: what crosses from
        /// the source Store to the destination Store.
        pub(crate) enum StreamRx {
            $(
                #[doc = concat!("A stream of `", stringify!($t), "`.")]
                $variant(Pipe<$t>),
            )*
        }

        /// Starts pumping `stream` out of its Store, returning the end its
        /// destination Store takes, and what tells the source Store the
        /// pump has ended.
        pub(crate) fn export_stream<D: IslandData>(
            acc: &Accessor<D>,
            stream: StreamAny,
            element: &Type,
            tap: Option<Tap>,
        ) -> wasmtime::Result<(StreamRx, Arc<Ended>)> {
            match element {
                $(Type::$variant => {
                    // A channel holds its buffer plus one item per sender.
                    let (tx, rx) = mpsc::channel::<Chunk<$t>>(CAPACITY - 1);
                    let ended = Arc::new(Ended::default());
                    let reader = StreamReader::<$t>::try_from_stream_any(stream)?;
                    let memory = acc.with(|mut access| {
                        let memory = access.data_mut().host_state().memory();
                        let sink = Sink { tx, tap, memory: memory.clone(), ended: ended.clone() };
                        reader.pipe(&mut access, sink).map(|()| memory)
                    })?;
                    let pipe = Pipe { rx, memory, ended: ended.clone() };
                    Ok((StreamRx::$variant(pipe), ended))
                })*
                other => wasmtime::bail!("a stream of {other:?} cannot leave its Store"),
            }
        }

        /// Makes a stream in this Store that yields what a pump brings, and
        /// the [`Intake`] the Store cuts once its share of the generation
        /// is done.
        pub(crate) fn import_stream<D: IslandData>(
            acc: &Accessor<D>,
            rx: StreamRx,
        ) -> wasmtime::Result<(Val, Intake)> {
            acc.with(|mut access| {
                let (stream, intake) = match rx {
                    $(StreamRx::$variant(pipe) => {
                        let received = Received { pipe, pending: VecDeque::new(), held: 0 };
                        let slot = Arc::new(Mutex::new(Some(received)));
                        let intake = Intake::of(&slot);
                        let stream = StreamReader::new(&mut access, Feed { slot })?
                            .try_into_stream_any(&mut access)?;
                        (stream, intake)
                    })*
                };
                Ok((Val::Stream(stream), intake))
            })
        }
    };
}

pumped!(
    Bool(Bool) => bool,
    S8(S8) => i8,
    U8(U8) => u8,
    S16(S16) => i16,
    U16(U16) => u16,
    S32(S32) => i32,
    U32(U32) => u32,
    S64(S64) => i64,
    U64(U64) => u64,
    Float32(F32) => f32,
    Float64(F64) => f64,
    Char(Char) => char,
    String(String) => String,
);
