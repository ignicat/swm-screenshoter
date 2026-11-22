//! Standalone SWM Unix-socket IPC client matching `swm-ipc` wire protocol v2.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const SWM_IPC_VERSION: u32 = 2;
pub const MAX_FRAME_LEN: usize = 1024 * 1024;
const MAX_RESPONSE_FDS: usize = 4;

#[derive(Debug)]
pub enum IpcError {
    Io(io::Error),
    Codec(postcard::Error),
    FrameTooLarge(usize),
    Server(String),
    Protocol(String),
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Codec(err) => write!(f, "IPC codec error: {err}"),
            Self::FrameTooLarge(len) => write!(f, "IPC frame too large: {len} bytes"),
            Self::Server(message) => write!(f, "{message}"),
            Self::Protocol(msg) => write!(f, "IPC protocol error: {msg}"),
        }
    }
}

impl std::error::Error for IpcError {}

impl From<io::Error> for IpcError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<postcard::Error> for IpcError {
    fn from(value: postcard::Error) -> Self {
        Self::Codec(value)
    }
}

pub fn default_socket_path() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(base).join("swm/swm.sock"))
}

// Variant order must stay 1:1 with `swm-ipc/src/types.rs` (postcard uses variant idx).

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    Outputs,
    Version,
    ChooseSource(SourceChooserRequest),
    CancelSourceChooser {
        request_handle: String,
    },
    RegisterDmabuf(RegisterDmabufRequest),
    RemoveDmabuf {
        stream_handle: String,
        buffer_id: u64,
    },
    CaptureFrame(CaptureFrameRequest),
    Node(NodeRequest),
    Quit,
    ConfigPath,
    Dpms {
        command: DpmsCommand,
        output: Option<String>,
    },
    CaptureCapabilities,
    Hello(HelloRequest),
    Subscribe(SubscribeRequest),
    ConfigReload,
    Trail(TrailRequest),
    Control(ControlRequest),
    BeginWindowCast {
        window_id: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    Outputs(OutputsResponse),
    Version(VersionInfo),
    Source(SourceChooserResponse),
    Frame(CaptureFrameResponse),
    Ack,
    NodeList(NodeListResponse),
    NodeInfo(NodeInfo),
    ConfigPath(Option<String>),
    CaptureCapabilities(CaptureCapabilities),
    Hello(ServerInfo),
    Subscribed(StateSnapshot),
    Event(ApiEvent),
    ApiError(ServerError),
    TrailList(TrailListResponse),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerErrorKind {
    InvalidRequest,
    NotFound,
    Ambiguous,
    Unsupported,
    VersionMismatch,
    Busy,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerError {
    pub kind: ServerErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloRequest {
    pub api_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub compositor_version: String,
    pub api_version: u32,
    pub ipc_protocol: u32,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventTopic {
    Outputs,
    Nodes,
    NodeGeometry,
    Config,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribeRequest {
    pub api_version: u32,
    pub topics: Vec<EventTopic>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub sequence: u64,
    pub outputs: Vec<OutputInfo>,
    pub nodes: Vec<NodeInfo>,
    pub config_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ApiEvent {
    OutputAdded {
        sequence: u64,
        output: OutputInfo,
    },
    OutputChanged {
        sequence: u64,
        output: OutputInfo,
    },
    OutputRemoved {
        sequence: u64,
        name: String,
    },
    NodeAdded {
        sequence: u64,
        node: NodeInfo,
    },
    NodeChanged {
        sequence: u64,
        node: NodeInfo,
    },
    NodeGeometryChanged {
        sequence: u64,
        id: u64,
        pos_x: f32,
        pos_y: f32,
        width: f32,
        height: f32,
    },
    NodeRemoved {
        sequence: u64,
        id: u64,
    },
    ConfigReloaded {
        sequence: u64,
        accepted: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeRequest {
    List {
        output: Option<String>,
    },
    Info {
        selector: Option<NodeSelector>,
        output: Option<String>,
    },
    Focus {
        selector: Option<NodeSelector>,
        output: Option<String>,
    },
    Move {
        direction: NodeMoveDirection,
        selector: Option<NodeSelector>,
        output: Option<String>,
    },
    Close {
        selector: Option<NodeSelector>,
        output: Option<String>,
    },
    Collapse {
        selector: Option<NodeSelector>,
        output: Option<String>,
    },
    Restore {
        selector: Option<NodeSelector>,
        output: Option<String>,
    },
    Toggle {
        selector: Option<NodeSelector>,
        output: Option<String>,
    },
    MoveBy {
        direction: NodeMoveDirection,
        distance: u32,
        selector: Option<NodeSelector>,
        output: Option<String>,
    },
    MoveTo {
        x: i32,
        y: i32,
        selector: Option<NodeSelector>,
        output: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeMoveDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeSelector {
    Focused,
    Latest,
    Id(u64),
    Title(String),
    App(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    Surface,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    Active,
    Drifting,
    Node,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeRole {
    NormalToplevel,
    Dialog,
    Popup,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeProtocolFamily {
    XdgToplevel,
    XdgPopup,
    Xwayland,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRelationInfo {
    pub node_id: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeScreenInfo {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub buffer_width: i32,
    pub buffer_height: i32,
    pub stack_order: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: u64,
    pub title: String,
    pub app_id: Option<String>,
    pub output: Option<String>,
    pub kind: NodeKind,
    pub state: NodeState,
    pub visible: bool,
    pub focused: bool,
    pub latest: bool,
    pub pinned: bool,
    pub role: NodeRole,
    pub protocol_family: NodeProtocolFamily,
    pub modal: bool,
    pub parent: Option<NodeRelationInfo>,
    pub transient_for: Option<NodeRelationInfo>,
    pub child_popup_count: usize,
    pub pos_x: f32,
    pub pos_y: f32,
    pub width: f32,
    pub height: f32,
    pub screen: Option<NodeScreenInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeOutputGroup {
    pub output: String,
    pub nodes: Vec<NodeInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeListResponse {
    pub outputs: Vec<NodeOutputGroup>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrailEntryInfo {
    pub index: usize,
    pub cursor: bool,
    pub node: NodeInfo,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrailListResponse {
    pub output: String,
    pub cursor_index: Option<usize>,
    pub entries: Vec<TrailEntryInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MonitorFocusTarget {
    Direction(ControlDirection),
    Output(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlRequest {
    MonitorFocus(MonitorFocusTarget),
    WindowTransfer(ControlDirection),
    PanField(ControlDirection),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceChooserRequest {
    pub request_handle: String,
    pub source_types: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureSource {
    Monitor {
        name: String,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    Window {
        window_id: u64,
        width: i32,
        height: i32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceChooserResponse {
    Selected(CaptureSource),
    Cancelled,
    Failed { message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DpmsCommand {
    Off,
    On,
    Toggle,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrailTarget {
    Index(usize),
    Selector(NodeSelector),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrailRequest {
    Previous {
        output: Option<String>,
    },
    Next {
        output: Option<String>,
    },
    List {
        output: Option<String>,
    },
    Goto {
        target: TrailTarget,
        output: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmabufFormat {
    pub fourcc: u32,
    pub modifier: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureCapabilities {
    pub main_device: Option<u64>,
    pub dmabuf_formats: Vec<DmabufFormat>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmabufPlane {
    pub fd_index: u32,
    pub plane_index: u32,
    pub offset: u32,
    pub stride: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterDmabufRequest {
    pub stream_handle: String,
    pub buffer_id: u64,
    pub width: i32,
    pub height: i32,
    pub format: u32,
    pub modifier: u64,
    pub flags: u32,
    pub planes: Vec<DmabufPlane>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureBuffer {
    MemFd {
        fd_index: u32,
        offset: u64,
        size: u64,
        stride: u32,
    },
    Dmabuf {
        buffer_id: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorMode {
    Hidden,
    Embedded,
    Metadata,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureFrameRequest {
    pub stream_handle: String,
    pub source: CaptureSource,
    pub cursor_mode: CursorMode,
    pub buffer: CaptureBuffer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorMetadata {
    pub x: i32,
    pub y: i32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureFrameResponse {
    pub cursor: Option<CursorMetadata>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputsResponse {
    pub outputs: Vec<OutputInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub name: String,
    pub modes: Vec<ModeInfo>,
    pub current_mode: Option<usize>,
    pub offset_x: i32,
    pub offset_y: i32,
    pub scale: f64,
    pub vrr: String,
    pub vrr_supported: bool,
    pub vrr_active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModeInfo {
    pub width: i32,
    pub height: i32,
    pub refresh_millihz: i32,
    pub preferred: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub ipc_protocol: u32,
}

#[derive(Clone, Debug)]
pub struct CapturedPixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct Connection {
    stream: UnixStream,
}

impl Connection {
    pub fn connect() -> Result<Self, IpcError> {
        Self::connect_to(default_socket_path()?)
    }

    pub fn connect_to(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        Ok(Self { stream })
    }

    pub fn request(&mut self, req: &Request, fds: &[RawFd]) -> Result<Response, IpcError> {
        let bytes = encode(req)?;
        write_frame_with_fds(&self.stream, &bytes, fds)?;
        let (reply_bytes, _reply_fds) = read_frame_with_fds(&self.stream, MAX_RESPONSE_FDS)?;
        decode(&reply_bytes)
    }

    pub fn version(&mut self) -> Result<VersionInfo, IpcError> {
        match self.request(&Request::Version, &[])? {
            Response::Version(v) => Ok(v),
            Response::ApiError(err) => Err(IpcError::Server(err.message)),
            other => Err(IpcError::Protocol(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub fn outputs(&mut self) -> Result<Vec<OutputInfo>, IpcError> {
        match self.request(&Request::Outputs, &[])? {
            Response::Outputs(o) => Ok(o.outputs),
            Response::ApiError(err) => Err(IpcError::Server(err.message)),
            other => Err(IpcError::Protocol(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub fn nodes(&mut self, output: Option<&str>) -> Result<Vec<NodeInfo>, IpcError> {
        let req = Request::Node(NodeRequest::List {
            output: output.map(str::to_owned),
        });
        match self.request(&req, &[])? {
            Response::NodeList(list) => {
                Ok(list.outputs.into_iter().flat_map(|g| g.nodes).collect())
            }
            Response::ApiError(err) => Err(IpcError::Server(err.message)),
            other => Err(IpcError::Protocol(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub fn config_path(&mut self) -> Result<Option<PathBuf>, IpcError> {
        match self.request(&Request::ConfigPath, &[])? {
            Response::ConfigPath(path) => Ok(path.map(PathBuf::from)),
            Response::ApiError(err) => Err(IpcError::Server(err.message)),
            other => Err(IpcError::Protocol(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub fn capture_frame(
        &mut self,
        source: CaptureSource,
        include_cursor: bool,
    ) -> Result<CapturedPixels, IpcError> {
        let (w_i32, h_i32) = match &source {
            CaptureSource::Monitor { width, height, .. }
            | CaptureSource::Window { width, height, .. } => (*width, *height),
        };
        if w_i32 <= 0 || h_i32 <= 0 {
            return Err(IpcError::Protocol(format!(
                "invalid capture dimensions: {w_i32}x{h_i32}"
            )));
        }
        let width = w_i32 as u32;
        let height = h_i32 as u32;
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| IpcError::Protocol("capture stride overflow".into()))?;
        let expected = (stride as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| IpcError::Protocol("pixel buffer size overflow".into()))?;

        let raw_fd =
            unsafe { libc::memfd_create(c"swm-screenshoter-frame".as_ptr(), libc::MFD_CLOEXEC) };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let len = libc::off_t::try_from(expected)
            .map_err(|_| IpcError::Protocol("pixel buffer exceeds off_t".into()))?;
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len) } < 0 {
            return Err(io::Error::last_os_error().into());
        }

        let req = Request::CaptureFrame(CaptureFrameRequest {
            stream_handle: "swm-screenshoter".into(),
            source,
            cursor_mode: if include_cursor {
                CursorMode::Embedded
            } else {
                CursorMode::Hidden
            },
            buffer: CaptureBuffer::MemFd {
                fd_index: 0,
                offset: 0,
                size: expected as u64,
                stride,
            },
        });

        match self.request(&req, &[fd.as_raw_fd()])? {
            Response::Frame(_) => {
                let mut file = std::fs::File::from(fd);
                let mut rgba = Vec::with_capacity(expected);
                file.read_to_end(&mut rgba)?;
                if rgba.len() != expected {
                    return Err(IpcError::Protocol(format!(
                        "pixel buffer has {} bytes, expected {expected}",
                        rgba.len()
                    )));
                }
                for px in rgba.as_chunks_mut::<4>().0 {
                    px.swap(0, 2);
                }
                Ok(CapturedPixels {
                    width,
                    height,
                    rgba,
                })
            }
            Response::ApiError(err) => Err(IpcError::Server(err.message)),
            other => Err(IpcError::Protocol(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }
}

pub fn verify_compositor(conn: &mut Connection) -> Result<VersionInfo, IpcError> {
    let info = conn.version()?;
    if info.ipc_protocol != SWM_IPC_VERSION {
        return Err(IpcError::Protocol(format!(
            "SWM IPC version mismatch: compositor uses v{}, swm-screenshoter expects v{SWM_IPC_VERSION}",
            info.ipc_protocol
        )));
    }
    Ok(info)
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, IpcError> {
    Ok(postcard::to_stdvec(value)?)
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, IpcError> {
    Ok(postcard::from_bytes(bytes)?)
}

pub fn write_frame_with_fds(
    stream: &UnixStream,
    payload: &[u8],
    fds: &[RawFd],
) -> Result<(), IpcError> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge(payload.len()));
    }
    let len_bytes = (payload.len() as u32).to_le_bytes();
    if fds.is_empty() {
        let mut s = stream;
        s.write_all(&len_bytes)?;
        s.write_all(payload)?;
        return Ok(());
    }
    let iov = [
        libc::iovec {
            iov_base: len_bytes.as_ptr().cast_mut().cast(),
            iov_len: len_bytes.len(),
        },
        libc::iovec {
            iov_base: payload.as_ptr().cast_mut().cast(),
            iov_len: payload.len(),
        },
    ];
    let fds_size = std::mem::size_of_val(fds);
    let space = (unsafe { libc::CMSG_SPACE(fds_size as _) }) as usize;
    let mut cmsg_buf = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_ptr().cast_mut();
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast();
    msg.msg_controllen = cmsg_buf.len() as _;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(fds_size as _) as _;
        std::ptr::copy_nonoverlapping(
            fds.as_ptr(),
            libc::CMSG_DATA(header).cast::<RawFd>(),
            fds.len(),
        );
    }
    let mut sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
    while sent < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
        sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
    }
    if sent < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let total = len_bytes.len() + payload.len();
    let sent = sent as usize;
    if sent < len_bytes.len() {
        let mut s = stream;
        s.write_all(&len_bytes[sent..])?;
        s.write_all(payload)?;
    } else if sent < total {
        let mut s = stream;
        s.write_all(&payload[sent - len_bytes.len()..])?;
    }
    Ok(())
}

pub fn read_frame_with_fds(
    stream: &UnixStream,
    max_fds: usize,
) -> Result<(Vec<u8>, Vec<OwnedFd>), IpcError> {
    let mut length = [0u8; 4];
    let mut iov = [libc::iovec {
        iov_base: length.as_mut_ptr().cast(),
        iov_len: length.len(),
    }];
    let space = if max_fds > 0 {
        (unsafe { libc::CMSG_SPACE((max_fds * std::mem::size_of::<RawFd>()) as _) }) as usize
    } else {
        0
    };
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_mut_ptr();
    msg.msg_iovlen = 1;
    if !control.is_empty() {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control.len() as _;
    }
    let mut received =
        unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    while received < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
        received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    }
    if received < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if received == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "IPC peer closed").into());
    }
    let mut fds = Vec::new();
    unsafe {
        let mut hdr = libc::CMSG_FIRSTHDR(&msg);
        while !hdr.is_null() {
            if (*hdr).cmsg_level == libc::SOL_SOCKET && (*hdr).cmsg_type == libc::SCM_RIGHTS {
                let data_len =
                    ((*hdr).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                let count = data_len / std::mem::size_of::<RawFd>();
                let ptr = libc::CMSG_DATA(hdr).cast::<RawFd>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(*ptr.add(i)));
                }
            }
            hdr = libc::CMSG_NXTHDR(&msg, hdr);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(IpcError::Protocol(
            "IPC descriptor message truncated".into(),
        ));
    }
    let received = received as usize;
    if received < length.len() {
        let mut s = stream;
        s.read_exact(&mut length[received..])?;
    }
    let payload_len = u32::from_le_bytes(length) as usize;
    if payload_len > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge(payload_len));
    }
    let mut payload = vec![0u8; payload_len];
    let mut s = stream;
    s.read_exact(&mut payload)?;
    Ok((payload, fds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn wire_request_variants_round_trip() {
        for req in [
            Request::Outputs,
            Request::Version,
            Request::ConfigPath,
            Request::Node(NodeRequest::List {
                output: Some("DP-1".into()),
            }),
            Request::CaptureFrame(CaptureFrameRequest {
                stream_handle: "swm-screenshoter".into(),
                source: CaptureSource::Window {
                    window_id: 7,
                    width: 800,
                    height: 600,
                },
                cursor_mode: CursorMode::Hidden,
                buffer: CaptureBuffer::MemFd {
                    fd_index: 0,
                    offset: 0,
                    size: 800 * 600 * 4,
                    stride: 800 * 4,
                },
            }),
        ] {
            let bytes = encode(&req).unwrap();
            let decoded: Request = decode(&bytes).unwrap();
            assert_eq!(format!("{decoded:?}"), format!("{req:?}"));
        }
    }

    #[test]
    fn scm_rights_memfd_transfer_round_trips() {
        let (server, client) = UnixStream::pair().unwrap();
        let data = b"RGBA_TEST_PIXELS";
        let raw_fd = unsafe { libc::memfd_create(c"test-cap".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw_fd >= 0);
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        unsafe {
            libc::ftruncate(fd.as_raw_fd(), data.len() as libc::off_t);
            libc::pwrite(fd.as_raw_fd(), data.as_ptr().cast(), data.len(), 0);
        }
        let resp = Response::Frame(CaptureFrameResponse { cursor: None });
        let bytes = encode(&resp).unwrap();
        write_frame_with_fds(&server, &bytes, &[fd.as_raw_fd()]).unwrap();
        drop(fd);

        let (reply_bytes, mut fds) = read_frame_with_fds(&client, 1).unwrap();
        let decoded: Response = decode(&reply_bytes).unwrap();
        assert!(matches!(
            decoded,
            Response::Frame(CaptureFrameResponse { cursor: None })
        ));
        let mut out = Vec::new();
        std::fs::File::from(fds.remove(0))
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);
    }
}
