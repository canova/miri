use std::borrow::Cow;
use std::convert::TryFrom;
use std::ffi::{OsStr, OsString};
use std::iter;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};

use rustc_target::abi::LayoutOf;

use crate::*;

/// Represent how path separator conversion should be done.
enum Pathconversion {
    HostToTarget,
    TargetToHost,
}

/// Perform path separator conversion if needed.
fn convert_path_separator<'a>(
    os_str: Cow<'a, OsStr>,
    target_os: &str,
    direction: Pathconversion,
) -> Cow<'a, OsStr> {
    #[cfg(windows)]
    return if target_os == "windows" {
        // Windows-on-Windows, all fine.
        os_str
    } else {
        // Unix target, Windows host.
        let (from, to) = match direction {
            Pathconversion::HostToTarget => ('\\', '/'),
            Pathconversion::TargetToHost => ('/', '\\'),
        };
        let converted = os_str
            .encode_wide()
            .map(|wchar| if wchar == from as u16 { to as u16 } else { wchar })
            .collect::<Vec<_>>();
        Cow::Owned(OsString::from_wide(&converted))
    };
    #[cfg(unix)]
    return if target_os == "windows" {
        // Windows target, Unix host.
        let (from, to) = match direction {
            Pathconversion::HostToTarget => ('/', '\\'),
            Pathconversion::TargetToHost => ('\\', '/'),
        };
        let converted = os_str
            .as_bytes()
            .iter()
            .map(|&wchar| if wchar == from as u8 { to as u8 } else { wchar })
            .collect::<Vec<_>>();
        Cow::Owned(OsString::from_vec(converted))
    } else {
        // Unix-on-Unix, all is fine.
        os_str
    };
}

impl<'mir, 'tcx: 'mir> EvalContextExt<'mir, 'tcx> for crate::MiriEvalContext<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: crate::MiriEvalContextExt<'mir, 'tcx> {
    /// Helper function to read an OsString from a null-terminated sequence of bytes, which is what
    /// the Unix APIs usually handle.
    fn read_os_str_from_c_str<'a>(&'a self, scalar: Scalar<Tag>) -> InterpResult<'tcx, &'a OsStr>
    where
        'tcx: 'a,
        'mir: 'a,
    {
        #[cfg(unix)]
        fn bytes_to_os_str<'tcx, 'a>(bytes: &'a [u8]) -> InterpResult<'tcx, &'a OsStr> {
            Ok(OsStr::from_bytes(bytes))
        }
        #[cfg(not(unix))]
        fn bytes_to_os_str<'tcx, 'a>(bytes: &'a [u8]) -> InterpResult<'tcx, &'a OsStr> {
            let s = std::str::from_utf8(bytes)
                .map_err(|_| err_unsup_format!("{:?} is not a valid utf-8 string", bytes))?;
            Ok(OsStr::new(s))
        }

        let this = self.eval_context_ref();
        let bytes = this.memory.read_c_str(scalar)?;
        bytes_to_os_str(bytes)
    }

    /// Helper function to read an OsString from a 0x0000-terminated sequence of u16,
    /// which is what the Windows APIs usually handle.
    fn read_os_str_from_wide_str<'a>(&'a self, scalar: Scalar<Tag>) -> InterpResult<'tcx, OsString>
    where
        'tcx: 'a,
        'mir: 'a,
    {
        #[cfg(windows)]
        pub fn u16vec_to_osstring<'tcx, 'a>(u16_vec: Vec<u16>) -> InterpResult<'tcx, OsString> {
            Ok(OsString::from_wide(&u16_vec[..]))
        }
        #[cfg(not(windows))]
        pub fn u16vec_to_osstring<'tcx, 'a>(u16_vec: Vec<u16>) -> InterpResult<'tcx, OsString> {
            let s = String::from_utf16(&u16_vec[..])
                .map_err(|_| err_unsup_format!("{:?} is not a valid utf-16 string", u16_vec))?;
            Ok(s.into())
        }

        let u16_vec = self.eval_context_ref().memory.read_wide_str(scalar)?;
        u16vec_to_osstring(u16_vec)
    }

    /// Helper function to write an OsStr as a null-terminated sequence of bytes, which is what
    /// the Unix APIs usually handle. This function returns `Ok((false, length))` without trying
    /// to write if `size` is not large enough to fit the contents of `os_string` plus a null
    /// terminator. It returns `Ok((true, length))` if the writing process was successful. The
    /// string length returned does not include the null terminator.
    fn write_os_str_to_c_str(
        &mut self,
        os_str: &OsStr,
        scalar: Scalar<Tag>,
        size: u64,
    ) -> InterpResult<'tcx, (bool, u64)> {
        #[cfg(unix)]
        fn os_str_to_bytes<'tcx, 'a>(os_str: &'a OsStr) -> InterpResult<'tcx, &'a [u8]> {
            Ok(os_str.as_bytes())
        }
        #[cfg(not(unix))]
        fn os_str_to_bytes<'tcx, 'a>(os_str: &'a OsStr) -> InterpResult<'tcx, &'a [u8]> {
            // On non-unix platforms the best we can do to transform bytes from/to OS strings is to do the
            // intermediate transformation into strings. Which invalidates non-utf8 paths that are actually
            // valid.
            os_str
                .to_str()
                .map(|s| s.as_bytes())
                .ok_or_else(|| err_unsup_format!("{:?} is not a valid utf-8 string", os_str).into())
        }

        let bytes = os_str_to_bytes(os_str)?;
        // If `size` is smaller or equal than `bytes.len()`, writing `bytes` plus the required null
        // terminator to memory using the `ptr` pointer would cause an out-of-bounds access.
        let string_length = u64::try_from(bytes.len()).unwrap();
        if size <= string_length {
            return Ok((false, string_length));
        }
        self.eval_context_mut()
            .memory
            .write_bytes(scalar, bytes.iter().copied().chain(iter::once(0u8)))?;
        Ok((true, string_length))
    }

    /// Helper function to write an OsStr as a 0x0000-terminated u16-sequence, which is what
    /// the Windows APIs usually handle. This function returns `Ok((false, length))` without trying
    /// to write if `size` is not large enough to fit the contents of `os_string` plus a null
    /// terminator. It returns `Ok((true, length))` if the writing process was successful. The
    /// string length returned does not include the null terminator.
    fn write_os_str_to_wide_str(
        &mut self,
        os_str: &OsStr,
        scalar: Scalar<Tag>,
        size: u64,
    ) -> InterpResult<'tcx, (bool, u64)> {
        #[cfg(windows)]
        fn os_str_to_u16vec<'tcx>(os_str: &OsStr) -> InterpResult<'tcx, Vec<u16>> {
            Ok(os_str.encode_wide().collect())
        }
        #[cfg(not(windows))]
        fn os_str_to_u16vec<'tcx>(os_str: &OsStr) -> InterpResult<'tcx, Vec<u16>> {
            // On non-Windows platforms the best we can do to transform Vec<u16> from/to OS strings is to do the
            // intermediate transformation into strings. Which invalidates non-utf8 paths that are actually
            // valid.
            os_str
                .to_str()
                .map(|s| s.encode_utf16().collect())
                .ok_or_else(|| err_unsup_format!("{:?} is not a valid utf-8 string", os_str).into())
        }

        let u16_vec = os_str_to_u16vec(os_str)?;
        // If `size` is smaller or equal than `bytes.len()`, writing `bytes` plus the required
        // 0x0000 terminator to memory would cause an out-of-bounds access.
        let string_length = u64::try_from(u16_vec.len()).unwrap();
        if size <= string_length {
            return Ok((false, string_length));
        }

        // Store the UTF-16 string.
        self.eval_context_mut()
            .memory
            .write_u16s(scalar, u16_vec.into_iter().chain(iter::once(0x0000)))?;
        Ok((true, string_length))
    }

    /// Allocate enough memory to store the given `OsStr` as a null-terminated sequence of bytes.
    fn alloc_os_str_as_c_str(
        &mut self,
        os_str: &OsStr,
        memkind: MemoryKind<MiriMemoryKind>,
    ) -> Pointer<Tag> {
        let size = u64::try_from(os_str.len()).unwrap().checked_add(1).unwrap(); // Make space for `0` terminator.
        let this = self.eval_context_mut();

        let arg_type = this.tcx.mk_array(this.tcx.types.u8, size);
        let arg_place = this.allocate(this.layout_of(arg_type).unwrap(), memkind);
        assert!(self.write_os_str_to_c_str(os_str, arg_place.ptr, size).unwrap().0);
        arg_place.ptr.assert_ptr()
    }

    /// Allocate enough memory to store the given `OsStr` as a null-terminated sequence of `u16`.
    fn alloc_os_str_as_wide_str(
        &mut self,
        os_str: &OsStr,
        memkind: MemoryKind<MiriMemoryKind>,
    ) -> Pointer<Tag> {
        let size = u64::try_from(os_str.len()).unwrap().checked_add(1).unwrap(); // Make space for `0x0000` terminator.
        let this = self.eval_context_mut();

        let arg_type = this.tcx.mk_array(this.tcx.types.u16, size);
        let arg_place = this.allocate(this.layout_of(arg_type).unwrap(), memkind);
        assert!(self.write_os_str_to_wide_str(os_str, arg_place.ptr, size).unwrap().0);
        arg_place.ptr.assert_ptr()
    }

    /// Read a null-terminated sequence of bytes, and perform path separator conversion if needed.
    fn read_path_from_c_str<'a>(&'a self, scalar: Scalar<Tag>) -> InterpResult<'tcx, Cow<'a, Path>>
    where
        'tcx: 'a,
        'mir: 'a,
    {
        let this = self.eval_context_ref();
        let os_str = this.read_os_str_from_c_str(scalar)?;

        Ok(match convert_path_separator(Cow::Borrowed(os_str), &this.tcx.sess.target.target.target_os, Pathconversion::TargetToHost) {
            Cow::Borrowed(x) => Cow::Borrowed(Path::new(x)),
            Cow::Owned(y) => Cow::Owned(PathBuf::from(y)),
        })
    }

    /// Read a null-terminated sequence of `u16`s, and perform path separator conversion if needed.
    fn read_path_from_wide_str(&self, scalar: Scalar<Tag>) -> InterpResult<'tcx, PathBuf> {
        let this = self.eval_context_ref();
        let os_str = this.read_os_str_from_wide_str(scalar)?;

        Ok(convert_path_separator(Cow::Owned(os_str), &this.tcx.sess.target.target.target_os, Pathconversion::TargetToHost).into_owned().into())
    }

    /// Write a Path to the machine memory (as a null-terminated sequence of bytes),
    /// adjusting path separators if needed.
    fn write_path_to_c_str(
        &mut self,
        path: &Path,
        scalar: Scalar<Tag>,
        size: u64,
    ) -> InterpResult<'tcx, (bool, u64)> {
        let this = self.eval_context_mut();
        let os_str = convert_path_separator(Cow::Borrowed(path.as_os_str()), &this.tcx.sess.target.target.target_os, Pathconversion::HostToTarget);
        this.write_os_str_to_c_str(&os_str, scalar, size)
    }

    /// Write a Path to the machine memory (as a null-terminated sequence of `u16`s),
    /// adjusting path separators if needed.
    fn write_path_to_wide_str(
        &mut self,
        path: &Path,
        scalar: Scalar<Tag>,
        size: u64,
    ) -> InterpResult<'tcx, (bool, u64)> {
        let this = self.eval_context_mut();
        let os_str = convert_path_separator(Cow::Borrowed(path.as_os_str()), &this.tcx.sess.target.target.target_os, Pathconversion::HostToTarget);
        this.write_os_str_to_wide_str(&os_str, scalar, size)
    }
}
