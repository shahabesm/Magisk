use core::ffi::c_char;
use std::{
    fs::{read_to_string, remove_file, rename, File},
    io::{BufWriter, Write},
    ops::{Deref, DerefMut},
    os::fd::FromRawFd,
    path::{Path, PathBuf},
    pin::Pin,
};

use quick_protobuf::{BytesReader, MessageRead, MessageWrite, Writer};

use base::{
    cstr, debug, libc::mkstemp, raw_cstr, Directory, LoggedError, LoggedResult, MappedFile,
    StringExt, Utf8CStr, WalkResult,
};

use crate::ffi::{clone_attr, prop_cb_exec, PropCb};
use crate::resetprop::proto::persistent_properties::{
    mod_PersistentProperties::PersistentPropertyRecord, PersistentProperties,
};

macro_rules! PERSIST_PROP_DIR {
    () => {
        "/data/property"
    };
}

macro_rules! PERSIST_PROP {
    () => {
        concat!(PERSIST_PROP_DIR!(), "/persistent_properties")
    };
}

trait PropCbExec {
    fn exec(&mut self, name: &Utf8CStr, value: &Utf8CStr);
}

impl PropCbExec for Pin<&mut PropCb> {
    fn exec(&mut self, name: &Utf8CStr, value: &Utf8CStr) {
        unsafe { prop_cb_exec(self.as_mut(), name.as_ptr(), value.as_ptr()) }
    }
}

impl Deref for PersistentProperties {
    type Target = Vec<PersistentPropertyRecord>;

    fn deref(&self) -> &Self::Target {
        &self.properties
    }
}

impl DerefMut for PersistentProperties {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.properties
    }
}

trait PropExt {
    fn find_index(&self, name: &Utf8CStr) -> Result<usize, usize>;
    fn find(&mut self, name: &Utf8CStr) -> LoggedResult<&mut PersistentPropertyRecord>;
}

impl PropExt for PersistentProperties {
    fn find_index(&self, name: &Utf8CStr) -> Result<usize, usize> {
        self.binary_search_by(|p| p.name.as_deref().cmp(&Some(name.deref())))
    }

    fn find(&mut self, name: &Utf8CStr) -> LoggedResult<&mut PersistentPropertyRecord> {
        if let Ok(idx) = self.find_index(name) {
            Ok(&mut self[idx])
        } else {
            Err(LoggedError::default())
        }
    }
}

fn check_proto() -> bool {
    Path::new(PERSIST_PROP!()).exists()
}

fn file_get_prop(name: &Utf8CStr) -> LoggedResult<String> {
    let path = PathBuf::new().join(PERSIST_PROP_DIR!()).join(name);
    let path = path.as_path();
    debug!("resetprop: read prop from [{}]\n", path.display());
    Ok(read_to_string(path)?)
}

fn file_set_prop(name: &Utf8CStr, value: Option<&Utf8CStr>) -> LoggedResult<()> {
    let path = PathBuf::new().join(PERSIST_PROP_DIR!()).join(name);
    let path = path.as_path();
    if let Some(value) = value {
        let mut tmp = String::from(concat!(PERSIST_PROP_DIR!(), ".prop.XXXXXX"));
        {
            let mut f = unsafe {
                let fd = mkstemp(tmp.as_mut_ptr() as *mut c_char);
                if fd < 0 {
                    return Err(Default::default());
                }
                File::from_raw_fd(fd)
            };
            f.write_all(value.as_bytes())?;
        }
        debug!("resetprop: write prop to [{}]\n", tmp);
        rename(tmp, path)?;
    } else {
        debug!("resetprop: unlink [{}]\n", path.display());
        remove_file(path)?;
    }
    Ok(())
}

fn proto_read_props() -> LoggedResult<PersistentProperties> {
    debug!("resetprop: decode with protobuf [{}]", PERSIST_PROP!());
    let m = MappedFile::open(cstr!(PERSIST_PROP!()))?;
    let m = m.as_ref();
    let mut r = BytesReader::from_bytes(m);
    let mut props = PersistentProperties::from_reader(&mut r, m)?;
    // Keep the list sorted for binary search
    props.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(props)
}

fn proto_write_props(props: &PersistentProperties) -> LoggedResult<()> {
    let mut tmp = String::from(concat!(PERSIST_PROP!(), ".XXXXXX"));
    tmp.nul_terminate();
    {
        let f = unsafe {
            let fd = mkstemp(tmp.as_mut_ptr().cast());
            if fd < 0 {
                return Err(Default::default());
            }
            File::from_raw_fd(fd)
        };
        debug!("resetprop: encode with protobuf [{}]", tmp);
        props.write_message(&mut Writer::new(BufWriter::new(f)))?;
    }
    unsafe {
        clone_attr(raw_cstr!(PERSIST_PROP!()), tmp.as_ptr().cast());
    }
    rename(tmp, PERSIST_PROP!())?;
    Ok(())
}

pub unsafe fn persist_get_prop(name: *const c_char, prop_cb: Pin<&mut PropCb>) {
    fn inner(name: *const c_char, mut prop_cb: Pin<&mut PropCb>) -> LoggedResult<()> {
        let name = unsafe { Utf8CStr::from_ptr(name)? };
        if check_proto() {
            let mut props = proto_read_props()?;
            if let Ok(PersistentPropertyRecord {
                name: Some(ref mut n),
                value: Some(ref mut v),
            }) = props.find(name)
            {
                prop_cb.exec(Utf8CStr::from_string(n), Utf8CStr::from_string(v));
            }
        } else {
            let mut value = file_get_prop(name)?;
            prop_cb.exec(name, Utf8CStr::from_string(&mut value));
            debug!("resetprop: found prop [{}] = [{}]", name, value);
        }
        Ok(())
    }
    inner(name, prop_cb).ok();
}

pub unsafe fn persist_get_props(prop_cb: Pin<&mut PropCb>) {
    fn inner(mut prop_cb: Pin<&mut PropCb>) -> LoggedResult<()> {
        if check_proto() {
            let mut props = proto_read_props()?;
            props.iter_mut().for_each(|p| {
                if let PersistentPropertyRecord {
                    name: Some(ref mut n),
                    value: Some(ref mut v),
                } = p
                {
                    prop_cb.exec(Utf8CStr::from_string(n), Utf8CStr::from_string(v));
                }
            });
        } else {
            let mut dir = Directory::open(cstr!(PERSIST_PROP_DIR!()))?;
            dir.for_all_file(|f| {
                if let Ok(name) = Utf8CStr::from_bytes(f.d_name().to_bytes()) {
                    if let Ok(mut value) = file_get_prop(name) {
                        prop_cb.exec(name, Utf8CStr::from_string(&mut value));
                    }
                }
                Ok(WalkResult::Continue)
            })?;
        }
        Ok(())
    }
    inner(prop_cb).ok();
}

pub unsafe fn persist_delete_prop(name: *const c_char) -> bool {
    fn inner(name: *const c_char) -> LoggedResult<()> {
        let name = unsafe { Utf8CStr::from_ptr(name)? };
        if check_proto() {
            let mut props = proto_read_props()?;
            if let Ok(idx) = props.find_index(name) {
                props.remove(idx);
                proto_write_props(&props)
            } else {
                Err(LoggedError::default())
            }
        } else {
            file_set_prop(name, None)
        }
    }
    inner(name).is_ok()
}
pub unsafe fn persist_set_prop(name: *const c_char, value: *const c_char) -> bool {
    unsafe fn inner(name: *const c_char, value: *const c_char) -> LoggedResult<()> {
        let name = Utf8CStr::from_ptr(name)?;
        let value = Utf8CStr::from_ptr(value)?;
        if check_proto() {
            let mut props = proto_read_props()?;
            match props.find_index(name) {
                Ok(idx) => props[idx].value = Some(value.to_string()),
                Err(idx) => props.insert(
                    idx,
                    PersistentPropertyRecord {
                        name: Some(name.to_string()),
                        value: Some(value.to_string()),
                    },
                ),
            }
            proto_write_props(&props)
        } else {
            file_set_prop(name, Some(value))
        }
    }
    inner(name, value).is_ok()
}
