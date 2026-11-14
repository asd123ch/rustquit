use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AllocAnyThread, AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{
    NSData, NSError, NSHTTPURLResponse, NSMutableURLRequest, NSObject, NSObjectProtocol, NSString,
    NSURL, NSURLRequest, NSURLResponse, NSURLSession, NSURLSessionConfiguration,
    NSURLSessionDataDelegate, NSURLSessionDataTask, NSURLSessionDelegate,
    NSURLSessionResponseDisposition, NSURLSessionTask, NSURLSessionTaskDelegate,
};

pub struct Response {
    pub body: Vec<u8>,
    pub final_url: String,
    pub status: u16,
}

struct RequestState {
    body: Vec<u8>,
    failure: Option<String>,
    response: Option<(u16, String)>,
    sender: Option<Sender<Result<Response, String>>>,
}

pub struct SessionDelegateIvars {
    max_response_bytes: usize,
    state: Mutex<RequestState>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AnyThread]
    #[name = "RQNoRedirectSessionDelegate"]
    #[ivars = SessionDelegateIvars]
    struct SessionDelegate;

    unsafe impl NSObjectProtocol for SessionDelegate {}
    unsafe impl NSURLSessionDelegate for SessionDelegate {}

    unsafe impl NSURLSessionTaskDelegate for SessionDelegate {
        #[unsafe(method(URLSession:task:willPerformHTTPRedirection:newRequest:completionHandler:))]
        unsafe fn will_redirect(
            &self,
            _session: &NSURLSession,
            _task: &NSURLSessionTask,
            _response: &NSHTTPURLResponse,
            _request: &NSURLRequest,
            completion_handler: &block2::DynBlock<dyn Fn(*mut NSURLRequest)>,
        ) {
            completion_handler.call((std::ptr::null_mut(),));
        }

        #[unsafe(method(URLSession:task:didCompleteWithError:))]
        fn did_complete(
            &self,
            session: &NSURLSession,
            _task: &NSURLSessionTask,
            error: Option<&NSError>,
        ) {
            let mut state = self
                .ivars()
                .state
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let Some(sender) = state.sender.take() else {
                return;
            };
            let result = if let Some(failure) = state.failure.take() {
                Err(failure)
            } else if let Some(error) = error {
                Err(error.localizedDescription().to_string())
            } else if let Some((status, final_url)) = state.response.take() {
                Ok(Response {
                    body: std::mem::take(&mut state.body),
                    final_url,
                    status,
                })
            } else {
                Err("the server returned no HTTP response".to_owned())
            };
            drop(state);
            let _ = sender.send(result);
            session.finishTasksAndInvalidate();
        }
    }

    unsafe impl NSURLSessionDataDelegate for SessionDelegate {
        #[unsafe(method(URLSession:dataTask:didReceiveResponse:completionHandler:))]
        unsafe fn did_receive_response(
            &self,
            _session: &NSURLSession,
            _data_task: &NSURLSessionDataTask,
            response: &NSURLResponse,
            completion_handler: &block2::DynBlock<dyn Fn(NSURLSessionResponseDisposition)>,
        ) {
            let object: &AnyObject = response;
            let parsed = object
                .downcast_ref::<NSHTTPURLResponse>()
                .ok_or_else(|| "the server returned a non-HTTP response".to_owned())
                .and_then(|http| {
                    let status = u16::try_from(http.statusCode())
                        .map_err(|_| "the server returned an invalid HTTP status".to_owned())?;
                    let final_url = response
                        .URL()
                        .and_then(|url| url.absoluteString())
                        .map(|url| url.to_string())
                        .ok_or_else(|| "the server returned no final URL".to_owned())?;
                    Ok((status, final_url))
                });
            let expected = response.expectedContentLength();
            let too_large =
                expected >= 0 && expected as u64 > self.ivars().max_response_bytes as u64;
            let mut state = self
                .ivars()
                .state
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let disposition = match (parsed, too_large) {
                (_, true) => {
                    state.failure = Some("the server response was too large".to_owned());
                    NSURLSessionResponseDisposition::Cancel
                }
                (Ok(response), false) => {
                    state.response = Some(response);
                    NSURLSessionResponseDisposition::Allow
                }
                (Err(error), false) => {
                    state.failure = Some(error);
                    NSURLSessionResponseDisposition::Cancel
                }
            };
            drop(state);
            completion_handler.call((disposition,));
        }

        #[unsafe(method(URLSession:dataTask:didReceiveData:))]
        fn did_receive_data(
            &self,
            _session: &NSURLSession,
            data_task: &NSURLSessionDataTask,
            data: &NSData,
        ) {
            let bytes = unsafe { data.as_bytes_unchecked() };
            let mut state = self
                .ivars()
                .state
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            if state.body.len().saturating_add(bytes.len()) > self.ivars().max_response_bytes {
                state.failure = Some("the server response was too large".to_owned());
                drop(state);
                data_task.cancel();
            } else {
                state.body.extend_from_slice(bytes);
            }
        }
    }
);

unsafe impl Send for SessionDelegate {}
unsafe impl Sync for SessionDelegate {}

impl SessionDelegate {
    fn new(max_response_bytes: usize, sender: Sender<Result<Response, String>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(SessionDelegateIvars {
            max_response_bytes,
            state: Mutex::new(RequestState {
                body: Vec::new(),
                failure: None,
                response: None,
                sender: Some(sender),
            }),
        });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct PendingRequest {
    receiver: Receiver<Result<Response, String>>,
    session: Retained<NSURLSession>,
    _task: Retained<NSURLSessionDataTask>,
    _delegate: Retained<SessionDelegate>,
}

impl PendingRequest {
    pub fn try_recv(&self) -> Result<Result<Response, String>, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.session.invalidateAndCancel();
    }
}

pub fn get(url: &str, max_response_bytes: usize) -> Result<PendingRequest, String> {
    let url = NSURL::URLWithString(&NSString::from_str(url))
        .ok_or_else(|| "invalid request URL".to_owned())?;
    let request = NSMutableURLRequest::requestWithURL(&url);
    request.setHTTPMethod(&NSString::from_str("GET"));
    request.setValue_forHTTPHeaderField(
        Some(&NSString::from_str("application/vnd.github+json")),
        &NSString::from_str("Accept"),
    );
    request.setValue_forHTTPHeaderField(
        Some(&NSString::from_str(concat!(
            "RustQuit/",
            env!("CARGO_PKG_VERSION")
        ))),
        &NSString::from_str("User-Agent"),
    );

    let configuration = NSURLSessionConfiguration::ephemeralSessionConfiguration();
    configuration.setHTTPShouldSetCookies(false);
    configuration.setHTTPCookieStorage(None);
    configuration.setURLCredentialStorage(None);
    configuration.setURLCache(None);
    configuration.setAllowsCellularAccess(true);
    configuration.setWaitsForConnectivity(false);
    configuration.setTimeoutIntervalForRequest(15.0);
    configuration.setTimeoutIntervalForResource(15.0);
    let (sender, receiver) = mpsc::channel();
    let delegate = SessionDelegate::new(max_response_bytes, sender);
    let protocol = ProtocolObject::from_ref(&*delegate);
    let session = unsafe {
        NSURLSession::sessionWithConfiguration_delegate_delegateQueue(
            &configuration,
            Some(protocol),
            None,
        )
    };
    let task = session.dataTaskWithRequest(&request);
    task.resume();

    Ok(PendingRequest {
        receiver,
        session,
        _task: task,
        _delegate: delegate,
    })
}
