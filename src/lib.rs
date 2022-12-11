use std::{
    collections::HashMap,
    ffi::OsString,
    fs::{self, File},
    os::unix::prelude::{AsRawFd, OpenOptionsExt, RawFd},
    path::Path,
    result,
    time::Duration,
};

use epoll::ControlOptions;
use inotify::{Inotify, WatchMask};
use input_linux::{
    sys::{input_event, timeval},
    EvdevHandle, EventKind, InputEvent, KeyEvent,
};
use inspector::ResultInspector;
use slotmap::{new_key_type, DenseSlotMap};

pub use input_linux::{Key, KeyState};

const INPUT_DIR: &'static str = "/dev/input/";

/// Error type for hkd.
#[derive(Debug)]
pub enum Error {
	Epoll(std::io::Error),
	Inotify(std::io::Error),
}

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Error::Epoll(err) =>
				write!(f, "Epoll error: {err}"),
			Error::Inotify(err) =>
				write!(f, "Inotify error: {err}"),
		}
	}
}

impl std::error::Error for Error { }

/// Specialized result for hkd.
type Result<T> = result::Result<T, Error>;

/// Hkd event type.
#[derive(Debug)]
pub enum Event {
	Key(Key, KeyState),
}

impl TryFrom<input_linux::Event> for Event {
	type Error = ();
	fn try_from(value: input_linux::Event) -> result::Result<Self, Self::Error> {
		match value {
			input_linux::Event::Key(KeyEvent { key, value, .. })
				=> Ok(Event::Key(key, value)),
			_ => Err(()),
		}
	}
}

fn read_evdev_handle<P>(path: P) -> std::io::Result<Option<EvdevHandle<File>>>
where
	P: AsRef<Path>
{
	let file = fs::OpenOptions::new()
			.read(true)
			.custom_flags(libc::O_NONBLOCK)
		.open(path)?;
	let handle = EvdevHandle::new(file);
	let event_bits = handle.event_bits()?;
	if event_bits.get(EventKind::Key) {
		Ok(Some(handle))
	} else {
		Ok(None)
	}
}

enum EpollState {
	Waiting,
	Processing {
		index: usize,
		len: usize,
		state: InputState,
	},
}

enum InputState {
	Reading,
	Processing {
		index: usize,
		len: usize,
	},
}

new_key_type! {
	struct EvdevHandleKey;
}

struct InputInner {
	epoll: RawFd,
	inotify: Inotify,
	
	fd_to_handle: HashMap<RawFd, EvdevHandleKey>,
	handles: DenseSlotMap<EvdevHandleKey, EvdevHandle<File>>,
}

impl InputInner {
	fn new() -> Result<Self> {
		let epoll = epoll::create(true)
			.map_err(|err| Error::Epoll(err))?;
		
		let mut inotify = Inotify::init()
			.map_err(|err| Error::Inotify(err))?;
		
		inotify.add_watch(INPUT_DIR, WatchMask::DELETE | WatchMask::CREATE)
			.map_err(|err| Error::Inotify(err))?;
		
		epoll::ctl(
			epoll,
			ControlOptions::EPOLL_CTL_ADD,
			inotify.as_raw_fd(),
			epoll::Event::new(epoll::Events::EPOLLIN, inotify.as_raw_fd() as u64),
		).map_err(|err| Error::Epoll(err))?;
		
		Ok(
			InputInner {
				epoll,
				inotify,
				
				fd_to_handle: HashMap::new(),
				handles: DenseSlotMap::with_key(),
			}
		)
	}
	
	fn add<P>(&mut self, path: P) -> Result<()>
	where
		P: AsRef<Path>,
		OsString: From<P>,
	{
		let Ok(Some(handle)) = read_evdev_handle(path.as_ref()) else { return Ok(()) };
		
		epoll::ctl(
			self.epoll,
			ControlOptions::EPOLL_CTL_ADD,
			handle.as_raw_fd(),
			epoll::Event::new(epoll::Events::EPOLLIN, handle.as_raw_fd() as u64),
		).map_err(|err| Error::Epoll(err))?;
		
		self.handles.insert_with_key(|key| {
			self.fd_to_handle.insert(handle.as_raw_fd(), key);
			handle
		});
		
		Ok(())
	}
	
	fn remove(&mut self, key: EvdevHandleKey) -> Result<()> {
		if let Some(handle) = self.handles.remove(key) {
			self.fd_to_handle.remove(&handle.as_raw_fd()).unwrap_or_else(|| unreachable!());
			epoll::ctl(
				self.epoll,
				ControlOptions::EPOLL_CTL_DEL,
				handle.as_raw_fd(),
				epoll::Event::new(epoll::Events::EPOLLIN, 0),
			).map_err(|err| Error::Epoll(err))?;
		}
		Ok(())
	}
}

struct InputBuf {
	epoll: [epoll::Event; 16],
	inotify: [u8; 1024],
	input: [input_event; 16],
}

pub struct Input {
	state: EpollState,
	inner: InputInner,
	buf: InputBuf,
}

impl Input {
	pub fn new() -> Result<Self> {
		let mut devices = Self {
			state: EpollState::Waiting,
			
			inner: InputInner::new()?,
			
			buf: InputBuf {
				epoll: [epoll::Event { events: 0, data: 0 }; 16],
				inotify: [0; 1024],
				input: [
					input_event {
						code: 0,
						value: 0,
						type_: 0,
						time: timeval {
							tv_sec: 0,
							tv_usec: 0
						}
					};
					16
				],
			},
		};
		
		fs::read_dir(INPUT_DIR).unwrap()
			.filter_map(|entry| entry
				.inspect_err(|err|
					log::warn!("Error while trying to read directory entry: {err}."))
				.ok()
			)
			.filter(|entry| entry.file_type()
				.as_ref()
				.inspect_err(|err|
					log::warn!("Error while trying to inspect filetype: {err}."))
				.map_or(false, std::os::unix::fs::FileTypeExt::is_char_device)
			)
			.for_each(|entry| {
				let _ = devices.inner.add(entry.path())
					.inspect_err(|err|
						log::warn!("Error while trying to add evdev handle to epoll: {err}."));
			});
		
		Ok(devices)
	}
	
	fn transition_state(
		state: EpollState,
		buf: &mut InputBuf,
		inner: &mut InputInner,
		
		event: &mut Option<Event>,
	) -> EpollState {
		match state {
			EpollState::Waiting => {
				let result = epoll::wait(inner.epoll, -1, buf.epoll.as_mut_slice());
				match result {
					Ok(epoll_len) => {
						EpollState::Processing {
							index: 0,
							len: epoll_len,
							state: InputState::Reading,
						}
					}
					Err(err) => {
						log::warn!("Error waiting on epoll: {err}.");
						EpollState::Waiting
					}
				}
			}
			EpollState::Processing { index, len, state } => {
				if index < len {
					match state {
						InputState::Reading => {
							enum Event { CanRead, Broken, Other }
							enum Source { Inotify, EvdevHandle(EvdevHandleKey), Other }
							
							let (event, source) = {
								let event = buf.epoll[index];
								
								let events = epoll::Events::from_bits_truncate(event.events);
								let fd = event.data as i32;
								(
									if events.intersects(epoll::Events::EPOLLIN) {
										Event::CanRead
									} else if events.intersects(epoll::Events::EPOLLHUP | epoll::Events::EPOLLERR) {
										Event::Broken
									} else {
										Event::Other
									}
									,
									if fd == inner.inotify.as_raw_fd() {
										Source::Inotify
									} else {
										if let Some(key) = inner.fd_to_handle.get(&fd) {
											Source::EvdevHandle(*key)
										} else {
											log::warn!("Unknown file descriptor from epoll: {fd}.");
											Source::Other
										}
									}
								)
							};
							
							match (event, source) {
								(Event::CanRead, Source::Inotify) => {
									while let Ok(events) = inner.inotify.read_events(buf.inotify.as_mut_slice()) {
										for event in events {
											if event.mask.contains(inotify::EventMask::CREATE) {
												if let Some(name) = event.name {
													// Wait for devices to settle. Otherwise accessing them sometimes
													// fails.
													std::thread::sleep(Duration::from_millis(100));
													inner.add(Path::new(INPUT_DIR).join(name))
														.unwrap_or_else(|err| panic!("Failed to add evdev handle to epoll: {err}"));
												}
											}
										}
									}
									EpollState::Processing {
										index: index + 1,
										len,
										state: InputState::Reading,
									}
								}
								(Event::CanRead, Source::EvdevHandle(key)) => {
									let input_len = inner.handles[key].read(buf.input.as_mut_slice())
										.inspect_err(|err|
											log::warn!("Error reading from evdev handle: {err}."))
										.unwrap_or(0);
									
									EpollState::Processing {
										index,
										len,
										state: InputState::Processing {
											index: 0,
											len: input_len,
										},
									}
								}
								(Event::Broken, Source::Inotify) => {
									panic!("Inotify broke.");
								}
								(Event::Broken, Source::EvdevHandle(key)) => {
									inner.remove(key)
										.unwrap_or_else(|err| panic!("Failed to remove evdev handle from epoll: {err}"));
									EpollState::Processing {
										index: index + 1,
										len,
										state: InputState::Reading,
									}
								}
								_ => {
									EpollState::Processing {
										index: index + 1,
										len,
										state: InputState::Reading,
									}
								}
							}
						}
						InputState::Processing { index: input_index, len: input_len } => {
							if input_index >= input_len {
								EpollState::Processing {
									index: index + 1,
									len,
									state: InputState::Reading,
								}
							} else {
								*event = 'event: {
									let raw = buf.input[input_index];
									let Ok(generic) = InputEvent::from_raw(&raw) else { break 'event None };
									let Ok(typed) = input_linux::Event::new(*generic) else { break 'event None };
									let Ok(simple) = Event::try_from(typed) else { break 'event None };
									Some(simple)
								};
								
								EpollState::Processing {
									index,
									len,
									state: InputState::Processing {
										index: input_index + 1,
										len: input_len,
									},
								}
							}
						}
					}
				} else {
					EpollState::Waiting
				}
			}
		}
	}
	
	pub fn read_event(&mut self) -> Event {
		let mut event = None;
		loop {
			replace_with::replace_with_or_abort(
				&mut self.state,
				|state| Input::transition_state(
					state,
					&mut self.buf,
					&mut self.inner,
					&mut event,
				)
			);
			
			if let Some(event) = event {
				return event;
			}
		}
	}
}
