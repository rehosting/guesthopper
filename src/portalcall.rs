use libc::{syscall, SYS_sendto};

// Define URegSize and RegSize as the unsigned/signed register size for the target architecture
#[cfg(target_pointer_width = "64")]
pub type URegSize = u64;
#[cfg(target_pointer_width = "64")]
pub type RegSize = i64;

#[cfg(not(target_pointer_width = "64"))]
pub type URegSize = u32;
#[cfg(not(target_pointer_width = "64"))]
pub type RegSize = i32;

const PORTAL_MAGIC: URegSize = 0xc1d1e1f1;

#[inline]
pub fn portal_call(user_magic: URegSize, argc: i32, args: &[u64]) -> RegSize {
    unsafe {
        syscall(
            SYS_sendto,
            PORTAL_MAGIC,
            user_magic,
            argc,
            args.as_ptr(),
            0,
            0,
        ) as RegSize
    }
}

#[inline]
#[allow(dead_code)]
pub fn portal_call0(user_magic: URegSize) -> RegSize {
    portal_call(user_magic, 0, &[])
}

#[inline]
#[allow(dead_code)]
pub fn portal_call1(user_magic: URegSize, a1: u64) -> RegSize {
    portal_call(user_magic, 1, &[a1])
}

#[inline]
#[allow(dead_code)]
pub fn portal_call2(user_magic: URegSize, a1: u64, a2: u64) -> RegSize {
    portal_call(user_magic, 2, &[a1, a2])
}

#[inline]
#[allow(dead_code)]
pub fn portal_call3(user_magic: URegSize, a1: u64, a2: u64, a3: u64) -> RegSize {
    portal_call(user_magic, 3, &[a1, a2, a3])
}

#[inline]
#[allow(dead_code)]
pub fn portal_call4(user_magic: URegSize, a1: u64, a2: u64, a3: u64, a4: u64) -> RegSize {
    portal_call(user_magic, 4, &[a1, a2, a3, a4])
}

#[inline]
#[allow(dead_code)]
pub fn portal_call5(user_magic: URegSize, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> RegSize {
    portal_call(user_magic, 5, &[a1, a2, a3, a4, a5])
}
