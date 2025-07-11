use std::{ffi::c_void, fmt};

use log::{debug, warn};
use proton_sdk_sys::{
    cancellation::CancellationTokenHandle, data::{AsyncCallback, AsyncCallbackWithProgress, ByteArray}, downloads::{self, raw, DownloaderHandle}, drive::DriveClientHandle, prost::Message, protobufs::{FileDownloadRequest, IntResponse, ToByteArray}
};
use proton_sdk_sys::protobufs::ProgressUpdate;
use crate::{cancellation::{self, CancellationToken}, drive::DriveClient};
use proton_sdk_sys::protobufs::FromByteArray;

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("SDK error: {0}")]
    SdkError(#[from] anyhow::Error),

    #[error("Protobuf error: {0}")]
    ProtobufError(#[from] proton_sdk_sys::protobufs::ProtoError),

    #[error("Downloader creation failed: {0}")]
    CreationFailed(String),

    #[error("Download operation failed: {0}")]
    DownloadFailed(String),

    #[error("Downloader creation timed out")]
    CreationTimeout,

    #[error("Download operation timed out")]
    DownloadTimeout,

    #[error("Downloader handle is null")]
    NullHandle,

    #[error("Invalid Drive client handle")]
    InvalidClient,
}

pub struct Downloader {
    handle: DownloaderHandle,
    _client: DriveClientHandle,
}

struct CombinedDownloadState<F>
where
    F: Fn(f32) + Send + 'static,
{
    result_sender: tokio::sync::oneshot::Sender<Result<Vec<u8>, DownloadError>>,
    progress_callback: Option<F>,
}

impl Downloader {
    pub async fn new(
        client: DriveClientHandle,
        cancellation_token: CancellationTokenHandle,
    ) -> Result<Self, DownloadError> {
        if client.is_null() {
            return Err(DownloadError::InvalidClient);
        }

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<DownloaderHandle, DownloadError>>();
        let tx = Box::new(tx);
        let tx_ptr = Box::into_raw(tx)
            as *mut tokio::sync::oneshot::Sender<Result<DownloaderHandle, DownloadError>>;

        extern "C" fn create_success_callback(state: *const c_void, response: ByteArray) {
            if !state.is_null() {
                unsafe {
                    let tx_ptr = state
                        as *mut tokio::sync::oneshot::Sender<
                            Result<DownloaderHandle, DownloadError>,
                        >;
                    let tx = Box::from_raw(tx_ptr);

                    let response = response.as_slice().to_vec();
                    let handle = match IntResponse::decode(&*response) {
                        Ok(value) => {
                            DownloaderHandle::from(value.value as isize)
                        },
                        Err(e) => DownloaderHandle::null()
                    };

                    debug!("Downloader created with handle: {:?}", handle);
                    let _ = tx.send(Ok(handle));
                }
            }
        }

        extern "C" fn create_failure_callback(state: *const c_void, error_data: ByteArray) {
            if !state.is_null() {
                unsafe {
                    let tx_ptr = state
                        as *mut tokio::sync::oneshot::Sender<
                            Result<DownloaderHandle, DownloadError>,
                        >;
                    let tx = Box::from_raw(tx_ptr);

                    let error_slice = error_data.as_slice();
                    let error_msg = if error_slice.is_empty() {
                        "Unknown downloader creation error".to_string()
                    } else {
                        String::from_utf8_lossy(error_slice).to_string()
                    };

                    log::error!("Downloader creation failed: {}", error_msg);
                    let _ = tx.send(Err(DownloadError::CreationFailed(error_msg)));
                }
            }
        }

        let async_callback = AsyncCallback::new(
            tx_ptr as *const c_void,
            Some(create_success_callback),
            Some(create_failure_callback),
            cancellation_token.raw(),
        );

        // Empty request as per API specification
        let empty_request = ByteArray::empty();

        let result = downloads::raw::downloader_create(client, empty_request, async_callback)
            .map_err(|e| DownloadError::SdkError(e))?;

        if result != 0 {
            // Clean up the leaked box if FFI failed immediately
            unsafe {
                let _ = Box::from_raw(tx_ptr);
            }
            return Err(DownloadError::CreationFailed(format!(
                "FFI call failed with code: {}",
                result
            )));
        }

        // Wait for async completion with timeout
        let downloader_handle =
            match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
                Ok(Ok(handle)) => handle,
                Ok(Err(e)) => return Err(DownloadError::CreationFailed(e.to_string())),
                Err(_) => return Err(DownloadError::CreationTimeout),
            }?;

        if downloader_handle.is_null() {
            return Err(DownloadError::NullHandle);
        }

        log::debug!("Downloader created successfully: {:?}", downloader_handle);

        Ok(Self {
            handle: downloader_handle,
            _client: client,
        })
    }

    /// Gets the handle for this downloader
    pub fn handle(&self) -> DownloaderHandle {
        self.handle
    }

    /// Checks if the downloader handle is valid
    pub fn is_valid(&self) -> bool {
        !self.handle.is_null()
    }

    /// Downloads a file with progress tracking
    ///
    /// # Arguments
    /// * `request` - The file download request specifying what to download
    /// * `progress_callback` - Optional callback for progress updates
    /// * `cancellation_token` - Token to cancel the download if needed
    ///
    /// # Returns
    /// The downloaded file data as bytes, or an error if download failed
    pub async fn download_file<F>(
        &self,
        request: FileDownloadRequest,
        progress_callback: Option<F>,
        cancellation_token: &CancellationToken,
    ) -> Result<Vec<u8>, DownloadError>
    where
        F: Fn(f32) + Send + 'static,
    {
        if self.handle.is_null() {
            return Err(DownloadError::NullHandle);
        }

        let proto_buf = request
            .to_proto_buffer()
            .map_err(|e| DownloadError::ProtobufError(e))?;

        let (tx, rx) = tokio::sync::oneshot::channel();

        let has_progress_callback = progress_callback.is_some();

        let combined_state = Box::leak(Box::new(CombinedDownloadState {
            result_sender: tx,
            progress_callback,
        })) as *mut CombinedDownloadState<F>;

        extern "C" fn download_success_callback<F>(
            state: *const std::ffi::c_void,
            response: ByteArray,
        ) where
            F: Fn(f32) + Send + 'static,
        {
            if !state.is_null() {
                unsafe {
                    let state_ptr = state as *mut CombinedDownloadState<F>;
                    let download_state = Box::from_raw(state_ptr);

                    let file_data = response.as_slice().to_vec();
                    log::debug!("File downloaded successfully: {} bytes", file_data.len());

                    let _ = download_state.result_sender.send(Ok(file_data));
                }
            }
        }

        extern "C" fn download_failure_callback<F>(
            state: *const std::ffi::c_void,
            error_data: ByteArray,
        ) where
            F: Fn(f32) + Send + 'static,
        {
            if !state.is_null() {
                unsafe {
                    let state_ptr = state as *mut CombinedDownloadState<F>;
                    let download_state = Box::from_raw(state_ptr);

                    let error_slice = error_data.as_slice();
                    let error_msg = if error_slice.is_empty() {
                        "Unknown download error".to_string()
                    } else {
                        String::from_utf8_lossy(error_slice).to_string()
                    };

                    log::error!("File download failed: {}", error_msg);
                    let _ = download_state
                        .result_sender
                        .send(Err(DownloadError::DownloadFailed(error_msg)));
                }
            }
        }

        extern "C" fn progress_callback_fn<F>(
            state: *const std::ffi::c_void,
            progress_data: ByteArray,
        ) where
            F: Fn(f32) + Send + 'static,
        {
            if !state.is_null() {
                unsafe {
                    let state_ptr = state as *const CombinedDownloadState<F>;
                    let download_state = &*state_ptr;
                    let bytes = progress_data.as_slice();
                    let progress = ProgressUpdate::from_bytes(bytes).expect("No progress update data");
                    if let Some(ref callback) = download_state.progress_callback {
                        callback((progress.bytes_completed as f32 / progress.bytes_in_total as f32));
                    }
                }
            }
        }

        let main_async_callback = AsyncCallback::new(
            combined_state as *const std::ffi::c_void,
            Some(download_success_callback::<F>),
            Some(download_failure_callback::<F>),
            cancellation_token.handle().raw(),
        );

        let progress_cb = if has_progress_callback {
            proton_sdk_sys::data::Callback::new(
                combined_state as *const std::ffi::c_void,
                Some(progress_callback_fn::<F>),
            )
        } else {
            proton_sdk_sys::data::Callback::new(std::ptr::null(), None)
        };

        let async_callback_with_progress = AsyncCallbackWithProgress {
            async_callback: main_async_callback,
            progress_callback: progress_cb,
        };

        let result = raw::downloader_download_file(
            self.handle,
            proto_buf.as_byte_array(),
            async_callback_with_progress,
        )
        .map_err(|e| DownloadError::SdkError(e))?;

        if result != 0 {
            // clean up leak
            unsafe {
                let _ = Box::from_raw(combined_state);
            }
            return Err(DownloadError::DownloadFailed(format!(
                "FFI call failed with code: {}",
                result
            )));
        }

        // 5 min timeout
        match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
            Ok(result) => match result {
                Ok(result) => result,
                Err(e) => Err(DownloadError::DownloadFailed(e.to_string())),
            },
            Err(_) => Err(DownloadError::DownloadTimeout),
        }
    }

    /// Downloads a file without progress tracking (simpler version)
    ///
    /// # Arguments
    /// * `request` - The file download request
    /// * `cancellation_token` - Token to cancel the download if needed
    ///
    /// # Returns
    /// The downloaded file data as bytes
    pub async fn download_file_simple(
        &self,
        request: FileDownloadRequest,
        cancellation_token: &CancellationToken,
    ) -> Result<Vec<u8>, DownloadError> {
        self.download_file(request, None::<fn(f32)>, cancellation_token)
            .await
    }

    /// Explicitly frees the downloader
    ///
    /// Note: This is automatically called when the Downloader is dropped,
    /// so you usually don't need to call this manually.
    pub fn free(self) -> Result<(), DownloadError> {
        if !self.handle.is_null() {
            raw::downloader_free(self.handle).map_err(|e| DownloadError::SdkError(e))?;
            log::debug!("Downloader freed successfully");
        }
        Ok(())
    }
}

impl fmt::Debug for Downloader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Downloader")
            .field("handle", &self.handle)
            .field("valid", &self.is_valid())
            .finish()
    }
}

impl Drop for Downloader {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            if let Err(e) = raw::downloader_free(self.handle) {
                warn!("Failed to free downloader in Drop: {}", e);
            } else {
                debug!("Downloader cleaned up automatically");
            }
        }
    }
}

pub struct DownloaderBuilder {
    client: DriveClientHandle,
    token: CancellationTokenHandle
}

impl DownloaderBuilder {
    pub fn new(client: &DriveClient) -> Self {
        Self { client: client.handle(), token: client.session().cancellation_token().handle() }
    }

    pub async fn build(
        self
    ) -> Result<Downloader, DownloadError> {
        Downloader::new(self.client, self.token).await
    }
}