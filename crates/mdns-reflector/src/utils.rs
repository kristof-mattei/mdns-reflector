// because `From::from` cannot be called in `const` yet
// having raw `as usize` is dangerous, as copy-pasting might
// do it on a `value_u64 as usize` on a 32-bit platform which truncates
pub const fn u32_to_usize(from: u32) -> usize {
    const _: () = assert!(
        size_of::<usize>() >= size_of::<u32>(),
        "rtnetlink doesn't support 16-bit, so we don't either"
    );

    #[expect(
        clippy::as_conversions,
        reason = "Validated that `u32` fits in `usize`"
    )]
    {
        from as usize
    }
}
