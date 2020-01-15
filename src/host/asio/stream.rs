extern crate asio_sys as sys;
extern crate num_traits;

use self::num_traits::PrimInt;
use super::Device;
use std;
use std::sync::atomic::{Ordering, AtomicBool};
use std::sync::Arc;
use super::parking_lot::Mutex;
use BackendSpecificError;
use BuildStreamError;
use Format;
use InputData;
use OutputData;
use PauseStreamError;
use PlayStreamError;
use Sample;
use SampleFormat;
use StreamError;

/// Sample types whose constant silent value is known.
trait Silence {
    const SILENCE: Self;
}

/// Constraints on the ASIO sample types.
trait AsioSample: Clone + Copy + Silence + std::ops::Add<Self, Output = Self> {
    fn to_cpal_sample<T: Sample>(&self) -> T;
    fn from_cpal_sample<T: Sample>(&T) -> Self;
}

// Used to keep track of whether or not the current current asio stream buffer requires
// being silencing before summing audio.
#[derive(Default)]
struct SilenceAsioBuffer {
    first: bool,
    second: bool,
}

pub struct Stream {
    playing: Arc<AtomicBool>,
    // Ensure the `Driver` does not terminate until the last stream is dropped.
    driver: Arc<sys::Driver>,
    asio_streams: Arc<Mutex<sys::AsioStreams>>,
    callback_id: sys::CallbackId,
}

impl Stream {
    pub fn play(&self) -> Result<(), PlayStreamError> {
        self.playing.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn pause(&self) -> Result<(), PauseStreamError> {
        self.playing.store(false, Ordering::SeqCst);
        Ok(())
    }
}

impl Device {
    pub fn build_input_stream<T, D, E>(
        &self,
        format: &Format,
        mut data_callback: D,
        _error_callback: E,
    ) -> Result<Stream, BuildStreamError>
    where
        T: Sample,
        D: FnMut(InputData<T>) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        assert_eq!(format.data_type, T::FORMAT, "sample type does not match `format.data_type`");
        let stream_type = self.driver.input_data_type().map_err(build_stream_err)?;

        // Ensure that the desired sample type is supported.
        let data_type = super::device::convert_data_type(&stream_type)
            .ok_or(BuildStreamError::FormatNotSupported)?;
        if format.data_type != data_type {
            return Err(BuildStreamError::FormatNotSupported);
        }

        let num_channels = format.channels.clone();
        let buffer_size = self.get_or_create_input_stream(format)?;
        let cpal_num_samples = buffer_size * num_channels as usize;

        // Create the buffer depending on the size of the data type.
        let len_bytes = cpal_num_samples * data_type.sample_size();
        let mut interleaved = vec![0u8; len_bytes];

        let stream_playing = Arc::new(AtomicBool::new(false));
        let playing = Arc::clone(&stream_playing);
        let asio_streams = self.asio_streams.clone();

        // Set the input callback.
        // This is most performance critical part of the ASIO bindings.
        let callback_id = self.driver.add_callback(move |buffer_index| unsafe {
            // If not playing return early.
            if !playing.load(Ordering::SeqCst) {
                return
            }

            // There is 0% chance of lock contention the host only locks when recreating streams.
            let stream_lock = asio_streams.lock();
            let ref asio_stream = match stream_lock.input {
                Some(ref asio_stream) => asio_stream,
                None => return,
            };

            /// 1. Write from the ASIO buffer to the interleaved CPAL buffer.
            /// 2. Deliver the CPAL buffer to the user callback.
            unsafe fn process_input_callback<A, B, D, F>(
                callback: &mut D,
                interleaved: &mut [u8],
                asio_stream: &sys::AsioStream,
                buffer_index: usize,
                from_endianness: F,
            )
            where
                A: AsioSample,
                B: Sample,
                D: FnMut(InputData<B>) + Send + 'static,
                F: Fn(A) -> A,
            {
                // 1. Write the ASIO channels to the CPAL buffer.
                let interleaved: &mut [B] = cast_slice_mut(interleaved);
                let n_channels = interleaved.len() / asio_stream.buffer_size as usize;
                for ch_ix in 0..n_channels {
                    let asio_channel = asio_channel_slice::<A>(asio_stream, buffer_index, ch_ix);
                    for (frame, s_asio) in interleaved.chunks_mut(n_channels).zip(asio_channel) {
                        frame[ch_ix] = from_endianness(*s_asio).to_cpal_sample();
                    }
                }

                // 2. Deliver the interleaved buffer to the callback.
                let data = InputData { buffer: interleaved };
                callback(data);
            }

            match (&stream_type, data_type) {
                (&sys::AsioSampleType::ASIOSTInt16LSB, SampleFormat::I16) => {
                    process_input_callback::<i16, T, _, _>(
                        &mut data_callback,
                        &mut interleaved,
                        asio_stream,
                        buffer_index as usize,
                        from_le,
                    );
                }
                (&sys::AsioSampleType::ASIOSTInt16MSB, SampleFormat::I16) => {
                    process_input_callback::<i16, T, _, _>(
                        &mut data_callback,
                        &mut interleaved,
                        asio_stream,
                        buffer_index as usize,
                        from_be,
                    );
                }

                // TODO: Handle endianness conversion for floats? We currently use the `PrimInt`
                // trait for the `to_le` and `to_be` methods, but this does not support floats.
                (&sys::AsioSampleType::ASIOSTFloat32LSB, SampleFormat::F32) |
                (&sys::AsioSampleType::ASIOSTFloat32MSB, SampleFormat::F32) => {
                    process_input_callback::<f32, T, _, _>(
                        &mut data_callback,
                        &mut interleaved,
                        asio_stream,
                        buffer_index as usize,
                        std::convert::identity::<f32>,
                    );
                }

                // TODO: Add support for the following sample formats to CPAL and simplify the
                // `process_output_callback` function above by removing the unnecessary sample
                // conversion function.
                (&sys::AsioSampleType::ASIOSTInt32LSB, SampleFormat::I16) => {
                    process_input_callback::<i32, T, _, _>(
                        &mut data_callback,
                        &mut interleaved,
                        asio_stream,
                        buffer_index as usize,
                        from_le,
                    );
                }
                (&sys::AsioSampleType::ASIOSTInt32MSB, SampleFormat::I16) => {
                    process_input_callback::<i32, T, _, _>(
                        &mut data_callback,
                        &mut interleaved,
                        asio_stream,
                        buffer_index as usize,
                        from_be,
                    );
                }
                // TODO: Handle endianness conversion for floats? We currently use the `PrimInt`
                // trait for the `to_le` and `to_be` methods, but this does not support floats.
                (&sys::AsioSampleType::ASIOSTFloat64LSB, SampleFormat::F32) |
                (&sys::AsioSampleType::ASIOSTFloat64MSB, SampleFormat::F32) => {
                    process_input_callback::<f64, T, _, _>(
                        &mut data_callback,
                        &mut interleaved,
                        asio_stream,
                        buffer_index as usize,
                        std::convert::identity::<f64>,
                    );
                }

                unsupported_format_pair => {
                    unreachable!("`build_input_stream` should have returned with unsupported \
                                 format {:?}", unsupported_format_pair)
                }
            }
        });

        let driver = self.driver.clone();
        let asio_streams = self.asio_streams.clone();

        // Immediately start the device?
        self.driver.start().map_err(build_stream_err)?;

        Ok(Stream {
            playing: stream_playing,
            driver,
            asio_streams,
            callback_id,
        })
    }

    pub fn build_output_stream<T, D, E>(
        &self,
        format: &Format,
        mut data_callback: D,
        _error_callback: E,
    ) -> Result<Stream, BuildStreamError>
    where
        T: Sample,
        D: FnMut(OutputData<T>) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        assert_eq!(format.data_type, T::FORMAT, "sample type does not match `format.data_type`");
        let stream_type = self.driver.output_data_type().map_err(build_stream_err)?;

        // Ensure that the desired sample type is supported.
        let data_type = super::device::convert_data_type(&stream_type)
            .ok_or(BuildStreamError::FormatNotSupported)?;
        if format.data_type != data_type {
            return Err(BuildStreamError::FormatNotSupported);
        }

        let num_channels = format.channels.clone();
        let buffer_size = self.get_or_create_output_stream(format)?;
        let cpal_num_samples = buffer_size * num_channels as usize;

        // Create buffers depending on data type.
        let len_bytes = cpal_num_samples * data_type.sample_size();
        let mut interleaved = vec![0u8; len_bytes];
        let mut silence_asio_buffer = SilenceAsioBuffer::default();

        let stream_playing = Arc::new(AtomicBool::new(false));
        let playing = Arc::clone(&stream_playing);
        let asio_streams = self.asio_streams.clone();

        let callback_id = self.driver.add_callback(move |buffer_index| unsafe {
            // If not playing, return early.
            if !playing.load(Ordering::SeqCst) {
                return
            }

            // There is 0% chance of lock contention the host only locks when recreating streams.
            let stream_lock = asio_streams.lock();
            let ref asio_stream = match stream_lock.output {
                Some(ref asio_stream) => asio_stream,
                None => return,
            };

            // Silence the ASIO buffer that is about to be used.
            //
            // This checks if any other callbacks have already silenced the buffer associated with
            // the current `buffer_index`.
            //
            // If not, we will silence it and set the opposite buffer half to unsilenced.
            let silence = match buffer_index {
                0 if !silence_asio_buffer.first => {
                    silence_asio_buffer.first = true;
                    silence_asio_buffer.second = false;
                    true
                }
                0 => false,
                1 if !silence_asio_buffer.second => {
                    silence_asio_buffer.second = true;
                    silence_asio_buffer.first = false;
                    true
                }
                1 => false,
                _ => unreachable!("ASIO uses a double-buffer so there should only be 2"),
            };

            /// 1. Render the given callback to the given buffer of interleaved samples.
            /// 2. If required, silence the ASIO buffer.
            /// 3. Finally, write the interleaved data to the non-interleaved ASIO buffer,
            ///    performing endianness conversions as necessary.
            unsafe fn process_output_callback<A, B, D, F>(
                callback: &mut D,
                interleaved: &mut [u8],
                silence_asio_buffer: bool,
                asio_stream: &sys::AsioStream,
                buffer_index: usize,
                to_endianness: F,
            )
            where
                A: Sample,
                B: AsioSample,
                D: FnMut(OutputData<A>) + Send + 'static,
                F: Fn(B) -> B,
            {
                // 1. Render interleaved buffer from callback.
                let interleaved: &mut [A] = cast_slice_mut(interleaved);
                callback(OutputData { buffer: interleaved });

                // 2. Silence ASIO channels if necessary.
                let n_channels = interleaved.len() / asio_stream.buffer_size as usize;
                if silence_asio_buffer {
                    for ch_ix in 0..n_channels {
                        let asio_channel =
                            asio_channel_slice_mut::<B>(asio_stream, buffer_index, ch_ix);
                        asio_channel.iter_mut().for_each(|s| *s = to_endianness(B::SILENCE));
                    }
                }

                // 3. Write interleaved samples to ASIO channels, one channel at a time.
                for ch_ix in 0..n_channels {
                    let asio_channel =
                        asio_channel_slice_mut::<B>(asio_stream, buffer_index, ch_ix);
                    for (frame, s_asio) in interleaved.chunks(n_channels).zip(asio_channel) {
                        *s_asio = *s_asio + to_endianness(B::from_cpal_sample(&frame[ch_ix]));
                    }
                }
            }

            match (data_type, &stream_type) {
                (SampleFormat::I16, &sys::AsioSampleType::ASIOSTInt16LSB) => {
                    process_output_callback::<T, i16, _, _>(
                        &mut data_callback,
                        &mut interleaved,
                        silence,
                        asio_stream,
                        buffer_index as usize,
                        to_le,
                    );
                }
                (SampleFormat::I16, &sys::AsioSampleType::ASIOSTInt16MSB) => {
                    process_output_callback::<T, i16, _, _>(
                        &mut data_callback,
                        &mut interleaved,
                        silence,
                        asio_stream,
                        buffer_index as usize,
                        to_be,
                    );
                }

                // TODO: Handle endianness conversion for floats? We currently use the `PrimInt`
                // trait for the `to_le` and `to_be` methods, but this does not support floats.
                (SampleFormat::F32, &sys::AsioSampleType::ASIOSTFloat32LSB) |
                (SampleFormat::F32, &sys::AsioSampleType::ASIOSTFloat32MSB) => {
                    process_output_callback::<T, f32, _, _>(
                        &mut data_callback,
                        &mut interleaved,
                        silence,
                        asio_stream,
                        buffer_index as usize,
                        std::convert::identity::<f32>,
                    );
                }

                // TODO: Add support for the following sample formats to CPAL and simplify the
                // `process_output_callback` function above by removing the unnecessary sample
                // conversion function.
                (SampleFormat::I16, &sys::AsioSampleType::ASIOSTInt32LSB) => {
                    process_output_callback::<T, i32, _, _>(
                        &mut data_callback,
                        &mut interleaved,
                        silence,
                        asio_stream,
                        buffer_index as usize,
                        to_le,
                    );
                }
                (SampleFormat::I16, &sys::AsioSampleType::ASIOSTInt32MSB) => {
                    process_output_callback::<T, i32, _, _>(
                        &mut data_callback,
                        &mut interleaved,
                        silence,
                        asio_stream,
                        buffer_index as usize,
                        to_be,
                    );
                }
                // TODO: Handle endianness conversion for floats? We currently use the `PrimInt`
                // trait for the `to_le` and `to_be` methods, but this does not support floats.
                (SampleFormat::F32, &sys::AsioSampleType::ASIOSTFloat64LSB) |
                (SampleFormat::F32, &sys::AsioSampleType::ASIOSTFloat64MSB) => {
                    process_output_callback::<T, f64, _, _>(
                        &mut data_callback,
                        &mut interleaved,
                        silence,
                        asio_stream,
                        buffer_index as usize,
                        std::convert::identity::<f64>,
                    );
                }

                unsupported_format_pair => {
                    unreachable!("`build_output_stream` should have returned with unsupported \
                                 format {:?}", unsupported_format_pair)
                }
            }
        });

        let driver = self.driver.clone();
        let asio_streams = self.asio_streams.clone();

        // Immediately start the device?
        self.driver.start().map_err(build_stream_err)?;

        Ok(Stream {
            playing: stream_playing,
            driver,
            asio_streams,
            callback_id,
        })
    }

    /// Create a new CPAL Input Stream.
    ///
    /// If there is no existing ASIO Input Stream it will be created.
    ///
    /// On success, the buffer size of the stream is returned.
    fn get_or_create_input_stream(
        &self,
        format: &Format,
    ) -> Result<usize, BuildStreamError> {
        match self.default_input_format() {
            Ok(f) => {
                let num_asio_channels = f.channels;
                check_format(&self.driver, format, num_asio_channels)
            },
            Err(_) => Err(BuildStreamError::FormatNotSupported),
        }?;
        let num_channels = format.channels as usize;
        let ref mut streams = *self.asio_streams.lock();
        // Either create a stream if thers none or had back the
        // size of the current one.
        match streams.input {
            Some(ref input) => Ok(input.buffer_size as usize),
            None => {
                let output = streams.output.take();
                self.driver
                    .prepare_input_stream(output, num_channels)
                    .map(|new_streams| {
                        let bs = match new_streams.input {
                            Some(ref inp) => inp.buffer_size as usize,
                            None => unreachable!(),
                        };
                        *streams = new_streams;
                        bs
                    }).map_err(|ref e| {
                        println!("Error preparing stream: {}", e);
                        BuildStreamError::DeviceNotAvailable
                    })
            }
        }
    }

    /// Create a new CPAL Output Stream.
    ///
    /// If there is no existing ASIO Output Stream it will be created.
    fn get_or_create_output_stream(
        &self,
        format: &Format,
    ) -> Result<usize, BuildStreamError> {
        match self.default_output_format() {
            Ok(f) => {
                let num_asio_channels = f.channels;
                check_format(&self.driver, format, num_asio_channels)
            },
            Err(_) => Err(BuildStreamError::FormatNotSupported),
        }?;
        let num_channels = format.channels as usize;
        let ref mut streams = *self.asio_streams.lock();
        // Either create a stream if thers none or had back the
        // size of the current one.
        match streams.output {
            Some(ref output) => Ok(output.buffer_size as usize),
            None => {
                let output = streams.output.take();
                self.driver
                    .prepare_output_stream(output, num_channels)
                    .map(|new_streams| {
                        let bs = match new_streams.output {
                            Some(ref out) => out.buffer_size as usize,
                            None => unreachable!(),
                        };
                        *streams = new_streams;
                        bs
                    }).map_err(|ref e| {
                        println!("Error preparing stream: {}", e);
                        BuildStreamError::DeviceNotAvailable
                    })
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.driver.remove_callback(self.callback_id);
    }
}

impl Silence for i16 {
    const SILENCE: Self = 0;
}

impl Silence for i32 {
    const SILENCE: Self = 0;
}

impl Silence for f32 {
    const SILENCE: Self = 0.0;
}

impl Silence for f64 {
    const SILENCE: Self = 0.0;
}

impl AsioSample for i16 {
    fn to_cpal_sample<T: Sample>(&self) -> T {
        T::from(self)
    }
    fn from_cpal_sample<T: Sample>(t: &T) -> Self {
        Sample::from(t)
    }
}

impl AsioSample for i32 {
    fn to_cpal_sample<T: Sample>(&self) -> T {
        let s = (*self >> 16) as i16;
        s.to_cpal_sample()
    }
    fn from_cpal_sample<T: Sample>(t: &T) -> Self {
        let s = i16::from_cpal_sample(t);
        (s as i32) << 16
    }
}

impl AsioSample for f32 {
    fn to_cpal_sample<T: Sample>(&self) -> T {
        T::from(self)
    }
    fn from_cpal_sample<T: Sample>(t: &T) -> Self {
        Sample::from(t)
    }
}

impl AsioSample for f64 {
    fn to_cpal_sample<T: Sample>(&self) -> T {
        let f = *self as f32;
        f.to_cpal_sample()
    }
    fn from_cpal_sample<T: Sample>(t: &T) -> Self {
        let f = f32::from_cpal_sample(t);
        f as f64
    }
}

/// Check whether or not the desired format is supported by the stream.
///
/// Checks sample rate, data type and then finally the number of channels.
fn check_format(
    driver: &sys::Driver,
    format: &Format,
    num_asio_channels: u16,
) -> Result<(), BuildStreamError> {
    let Format {
        channels,
        sample_rate,
        data_type,
    } = format;
    // Try and set the sample rate to what the user selected.
    let sample_rate = sample_rate.0.into();
    if sample_rate != driver.sample_rate().map_err(build_stream_err)? {
        if driver.can_sample_rate(sample_rate).map_err(build_stream_err)? {
            driver
                .set_sample_rate(sample_rate)
                .map_err(build_stream_err)?;
        } else {
            return Err(BuildStreamError::FormatNotSupported);
        }
    }
    // unsigned formats are not supported by asio
    match data_type {
        SampleFormat::I16 | SampleFormat::F32 => (),
        SampleFormat::U16 => return Err(BuildStreamError::FormatNotSupported),
    }
    if *channels > num_asio_channels {
        return Err(BuildStreamError::FormatNotSupported);
    }
    Ok(())
}

/// Cast a byte slice into a mutable slice of desired type.
///
/// Safety: it's up to the caller to ensure that the input slice has valid bit representations.
unsafe fn cast_slice_mut<T>(v: &mut [u8]) -> &mut [T] {
    debug_assert!(v.len() % std::mem::size_of::<T>() == 0);
    std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut T, v.len() / std::mem::size_of::<T>())
}

/// Helper function to convert to little endianness.
fn to_le<T: PrimInt>(t: T) -> T {
    t.to_le()
}

/// Helper function to convert to big endianness.
fn to_be<T: PrimInt>(t: T) -> T {
    t.to_be()
}

/// Helper function to convert from little endianness.
fn from_le<T: PrimInt>(t: T) -> T {
    T::from_le(t)
}

/// Helper function to convert from little endianness.
fn from_be<T: PrimInt>(t: T) -> T {
    T::from_be(t)
}

/// Shorthand for retrieving the asio buffer slice associated with a channel.
///
/// Safety: it's up to the user to ensure that this function is not called multiple times for the
/// same channel.
unsafe fn asio_channel_slice<T>(
    asio_stream: &sys::AsioStream,
    buffer_index: usize,
    channel_index: usize,
) -> &[T] {
    asio_channel_slice_mut(asio_stream, buffer_index, channel_index)
}

/// Shorthand for retrieving the asio buffer slice associated with a channel.
///
/// Safety: it's up to the user to ensure that this function is not called multiple times for the
/// same channel.
unsafe fn asio_channel_slice_mut<T>(
    asio_stream: &sys::AsioStream,
    buffer_index: usize,
    channel_index: usize,
) -> &mut [T] {
    let buff_ptr: *mut T = asio_stream
        .buffer_infos[channel_index]
        .buffers[buffer_index as usize]
        as *mut _;
    std::slice::from_raw_parts_mut(buff_ptr, asio_stream.buffer_size as usize)
}

fn build_stream_err(e: sys::AsioError) -> BuildStreamError {
    match e {
        sys::AsioError::NoDrivers |
        sys::AsioError::HardwareMalfunction => BuildStreamError::DeviceNotAvailable,
        sys::AsioError::InvalidInput |
        sys::AsioError::BadMode => BuildStreamError::InvalidArgument,
        err => {
            let description = format!("{}", err);
            BackendSpecificError { description }.into()
        }
    }
}
