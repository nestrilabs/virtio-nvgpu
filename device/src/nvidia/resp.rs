//! Writing responses.

use super::*;

impl NvidiaBackend {
    // ------------------------------------------------------------------
    // Response helpers
    // ------------------------------------------------------------------

    /// Write a bare response header.
    pub(super) fn write_hdr(&self, resp_buf: &mut [u8], handle: u32, status: i32) -> usize {
        if resp_buf.len() < size_of::<MsgHeader>() {
            return 0;
        }
        let hdr = MsgHeader {
            msg_type: self.current_msg as u32,
            handle,
            status,
            padding: 0,
        };
        write_struct(resp_buf, &hdr)
    }

    /// Write a successful ioctl response: header, lengths, then the bytes.
    ///
    /// The split between the top-level struct and the nested block is taken
    /// from the request, because the guest copies exactly `data_len` bytes back
    /// to the caller's struct and reads any nested block after it.
    pub(super) fn write_ioctl_resp(
        &self,
        resp_buf: &mut [u8],
        cookie: u64,
        param_out: &[u8],
    ) -> usize {
        self.write_ioctl_resp_deep(resp_buf, cookie, param_out, 0)
    }

    /// As `write_ioctl_resp`, where the last `deep_len` bytes of `param_out`
    /// are what a pointer inside the nested block addresses, and are declared
    /// separately so the guest knows to copy them somewhere else.
    pub(super) fn write_ioctl_resp_deep(
        &self,
        resp_buf: &mut [u8],
        cookie: u64,
        param_out: &[u8],
        deep_len: usize,
    ) -> usize {
        let data_len = (self.current_data_len as usize).min(param_out.len());
        let deep_len = deep_len.min(param_out.len() - data_len);
        let nested_len = param_out.len() - data_len - deep_len;

        let need = size_of::<MsgHeader>() + size_of::<IoctlResp>() + param_out.len();
        if resp_buf.len() < need {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, cookie, 0);
        }

        let mut off = 0;
        off += write_struct(
            &mut resp_buf[off..],
            &MsgHeader {
                msg_type: self.current_msg as u32,
                handle: self.current_handle,
                status: 0,
                padding: 0,
            },
        );
        off += write_struct(
            &mut resp_buf[off..],
            &IoctlResp {
                data_len: data_len as u32,
                nested_len: nested_len as u32,
                deep_len: deep_len as u32,
            },
        );
        resp_buf[off..off + param_out.len()].copy_from_slice(param_out);
        off + param_out.len()
    }

    /// Write a failure.
    ///
    /// `status` is negative in the response because the driver tests
    /// `(s32)status < 0` and returns it straight out of the syscall. A positive
    /// value here reads as success and userspace proceeds on a failed call.
    pub(super) fn write_error_resp(
        &self,
        resp_buf: &mut [u8],
        status: Status,
        _cookie: u64,
        errno: i32,
    ) -> usize {
        let e = if errno != 0 {
            errno.abs()
        } else {
            status.errno()
        };
        self.write_hdr(resp_buf, 0, -e)
    }
}
