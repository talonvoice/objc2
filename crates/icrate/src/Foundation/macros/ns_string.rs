#![cfg(feature = "Foundation_NSString")]
/// Create a [`NSString`] from a static [`str`].
///
/// Equivalent to the [boxed C-strings] `@"string"` syntax in Objective-C.
///
/// [`NSString`]: crate::Foundation::NSString
/// [boxed C-strings]: https://clang.llvm.org/docs/ObjectiveCLiterals.html#boxed-c-strings
///
///
/// # Specification
///
/// The macro takes any expression that evaluates to a `const` `&str`, and
/// produces a `&'static NSString`.
///
/// The returned string's encoding is not guaranteed to be UTF-8.
///
/// Strings containing ASCII NUL is allowed, the NUL is preserved as one would
/// expect.
///
///
/// # Cargo features
///
/// If the experimental `"unstable-static-nsstring"` feature is enabled, this
/// will emit statics placed in special sections that will be replaced by dyld
/// when the program starts up - which will in turn will cause the runtime
/// cost of this macro to be completely non-existant!
///
/// However, it is known to not be completely reliable yet, see [#258] for
/// details.
///
/// [#258]: https://github.com/madsmtm/objc2/issues/258
///
///
/// # Examples
///
/// Creating a static `NSString`.
///
/// ```
/// use icrate::ns_string;
/// use icrate::Foundation::NSString;
///
/// let hello: &'static NSString = ns_string!("hello");
/// assert_eq!(hello.to_string(), "hello");
/// ```
///
/// Creating a `NSString` from a `const` `&str`.
///
/// ```
/// # use icrate::ns_string;
/// const EXAMPLE: &str = "example";
/// assert_eq!(ns_string!(EXAMPLE).to_string(), EXAMPLE);
/// ```
///
/// Creating unicode strings.
///
/// ```
/// # use icrate::ns_string;
/// let hello_ru = ns_string!("Привет");
/// assert_eq!(hello_ru.to_string(), "Привет");
/// ```
///
/// Creating a string containing a NUL byte:
///
/// ```
/// # use icrate::ns_string;
/// assert_eq!(ns_string!("example\0").to_string(), "example\0");
/// assert_eq!(ns_string!("exa\0mple").to_string(), "exa\0mple");
/// ```
#[cfg(feature = "Foundation_NSString")] // For auto_doc_cfg
#[macro_export]
macro_rules! ns_string {
    ($s:expr) => {{
        // Immediately place in constant for better UI
        const INPUT: &str = $s;
        $crate::__ns_string_inner!(INPUT)
    }};
}

#[doc(hidden)]
#[cfg(all(feature = "apple", feature = "unstable-static-nsstring"))]
#[macro_export]
macro_rules! __ns_string_inner {
    ($inp:ident) => {{
        const X: &[u8] = $inp.as_bytes();
        $crate::__ns_string_inner!(@inner X);
        // Return &'static NSString
        CFSTRING.as_nsstring()
    }};
    (@inner $inp:ident) => {
        // Note: We create both the ASCII + NUL and the UTF-16 + NUL versions
        // of the string, since we can't conditionally create a static.
        //
        // Since we don't add the `#[used]` attribute, Rust can fairly easily
        // figure out that one of the variants are never used, and simply
        // exclude it.

        // Convert the input slice to a C-style string with a NUL byte.
        //
        // The section is the same as what clang sets, see:
        // https://github.com/llvm/llvm-project/blob/release/13.x/clang/lib/CodeGen/CodeGenModule.cpp#L5192
        #[link_section = "__TEXT,__cstring,cstring_literals"]
        static ASCII: [u8; $inp.len() + 1] = {
            // Zero-fill with $inp.len() + 1
            let mut res: [u8; $inp.len() + 1] = [0; $inp.len() + 1];
            let mut i = 0;
            // Fill with data from $inp
            while i < $inp.len() {
                res[i] = $inp[i];
                i += 1;
            }
            // Now contains $inp + '\0'
            res
        };

        // The full UTF-16 contents along with the written length.
        const UTF16_FULL: (&[u16; $inp.len()], usize) = {
            let mut out = [0u16; $inp.len()];
            let mut iter = $crate::Foundation::__macro_helpers::EncodeUtf16Iter::new($inp);
            let mut written = 0;

            while let Some((state, chars)) = iter.next() {
                iter = state;
                out[written] = chars.repr[0];
                written += 1;

                if chars.len > 1 {
                    out[written] = chars.repr[1];
                    written += 1;
                }
            }

            (&{ out }, written)
        };

        // Convert the slice to an UTF-16 array + a final NUL byte.
        //
        // The section is the same as what clang sets, see:
        // https://github.com/llvm/llvm-project/blob/release/13.x/clang/lib/CodeGen/CodeGenModule.cpp#L5193
        #[link_section = "__TEXT,__ustring"]
        static UTF16: [u16; UTF16_FULL.1 + 1] = {
            // Zero-fill with UTF16_FULL.1 + 1
            let mut res: [u16; UTF16_FULL.1 + 1] = [0; UTF16_FULL.1 + 1];
            let mut i = 0;
            // Fill with data from UTF16_FULL.0 up until UTF16_FULL.1
            while i < UTF16_FULL.1 {
                res[i] = UTF16_FULL.0[i];
                i += 1;
            }
            // Now contains UTF16_FULL.1 + NUL
            res
        };

        // Create the constant string structure, and store it in a static
        // within a special section.
        //
        // The section is the same as what clang sets, see:
        // https://github.com/llvm/llvm-project/blob/release/13.x/clang/lib/CodeGen/CodeGenModule.cpp#L5243
        #[link_section = "__DATA,__cfstring"]
        static CFSTRING: $crate::Foundation::__macro_helpers::CFConstString = unsafe {
            if $crate::Foundation::__macro_helpers::is_ascii_no_nul($inp) {
                // This is technically an optimization (UTF-16 strings are
                // always valid), but it's a fairly important one!
                $crate::Foundation::__macro_helpers::CFConstString::new_ascii(
                    &$crate::Foundation::__macro_helpers::__CFConstantStringClassReference,
                    &ASCII,
                )
            } else {
                $crate::Foundation::__macro_helpers::CFConstString::new_utf16(
                    &$crate::Foundation::__macro_helpers::__CFConstantStringClassReference,
                    &UTF16,
                )
            }
        };
    };
}

#[doc(hidden)]
#[cfg(not(all(feature = "apple", feature = "unstable-static-nsstring")))]
#[macro_export]
macro_rules! __ns_string_inner {
    ($inp:ident) => {{
        static CACHED_NSSTRING: $crate::Foundation::__macro_helpers::CachedId<
            $crate::Foundation::NSString,
        > = $crate::Foundation::__macro_helpers::CachedId::new();
        CACHED_NSSTRING.get(|| $crate::Foundation::NSString::from_str($inp))
    }};
}
