use libc::{syscall, SYS_sendto};

#[cfg(target_pointer_width = "64")]
const PORTAL_MAGIC: libc::c_ulong = 0xc1d1e1f1;
#[cfg(not(target_pointer_width = "64"))]
const PORTAL_MAGIC: libc::c_int = 0xc1d1e1f1;

#[inline]
pub fn portal_call(user_magic: u64, argc: i32, args: &[u64]) -> i64 {
    unsafe {
        syscall(
            SYS_sendto,
            PORTAL_MAGIC,
            user_magic,
            argc,
            args.as_ptr(),
            0,
            0,
        ) as i64
    }
}

#[inline]
pub fn portal_call1(user_magic: u64, a1: u64) -> i64 {
    portal_call(user_magic, 1, &[a1])
}

#[inline]
pub fn portal_call2(user_magic: u64, a1: u64, a2: u64) -> i64 {
    portal_call(user_magic, 2, &[a1, a2])
}

#[inline]
pub fn portal_call3(user_magic: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    portal_call(user_magic, 3, &[a1, a2, a3])
}

#[inline]
pub fn portal_call4(user_magic: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    portal_call(user_magic, 4, &[a1, a2, a3, a4])
}

#[inline]
pub fn portal_call5(user_magic: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    portal_call(user_magic, 5, &[a1, a2, a3, a4, a5])
}
