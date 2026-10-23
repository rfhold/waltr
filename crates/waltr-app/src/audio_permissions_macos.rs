//! Audio-only `AVFoundation` boundary, supported on Apple Silicon macOS only.
#![allow(unsafe_code)]
use super::{PermissionBackend, PermissionReply, PermissionResult};
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

pub(super) struct MacPermission(pub(super) tauri::AppHandle);
impl PermissionBackend for MacPermission {
    fn request(&self, _: u64, reply: PermissionReply) {
        let failure = reply.clone();
        // Conservative host policy: initiate authorization on the main queue.
        if self
            .0
            .run_on_main_thread(move || request_on_main(reply))
            .is_err()
        {
            failure.complete(PermissionResult::Unavailable);
        }
    }
}
fn request_on_main(reply: PermissionReply) {
    if !reply.is_current() {
        reply.complete(PermissionResult::Cancelled);
        return;
    }
    // SAFETY: AVMediaTypeAudio is Apple's immutable process-lifetime NSString
    // constant. Only audio is queried. No device, session, or borrowed object is
    // retained. AVFoundation permits these class methods on this queue.
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
        reply.complete(PermissionResult::Unavailable);
        return;
    };
    // SAFETY: the checked process-lifetime audio constant satisfies the API.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) };
    if let Some(result) = classify(status) {
        reply.complete(result);
    } else {
        // The OS may copy/invoke this block on any queue. Its only capture
        // is owned PermissionReply (Weak<Mutex<...>>), which is Send + Sync;
        // it contains no borrowed runtime, capture, JNI, or thread handles.
        let block = block2::RcBlock::new(move |granted: objc2::runtime::Bool| {
            reply.complete(if granted.as_bool() {
                PermissionResult::Granted
            } else {
                PermissionResult::Denied
            });
        });
        // SAFETY: immutable audio constant as above; the typed block has the
        // required ABI and AVFoundation copies it for asynchronous ownership.
        // Cancellation invalidates the waiter; it cannot dismiss OS UI.
        unsafe {
            AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &block);
        }
    }
}
const fn classify(status: AVAuthorizationStatus) -> Option<PermissionResult> {
    match status {
        AVAuthorizationStatus::Authorized => Some(PermissionResult::Granted),
        AVAuthorizationStatus::NotDetermined => None,
        _ => Some(PermissionResult::Denied),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authorization_status_mapping_is_audio_only_and_fail_closed() {
        assert_eq!(
            classify(AVAuthorizationStatus::Authorized),
            Some(PermissionResult::Granted)
        );
        assert_eq!(classify(AVAuthorizationStatus::NotDetermined), None);
        for status in [
            AVAuthorizationStatus::Denied,
            AVAuthorizationStatus::Restricted,
            AVAuthorizationStatus(999),
        ] {
            assert_eq!(classify(status), Some(PermissionResult::Denied));
        }
        fn send_sync<T: Send + Sync>() {}
        send_sync::<PermissionReply>();
    }
}
