//! Stream identity and capabilities, independent of displayed benchmark names.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Tokio,
    Compio,
}
impl Runtime {
    pub fn suite(self) -> &'static str {
        match self {
            Self::Tokio => "stream_tokio",
            Self::Compio => "stream_compio",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    PlainUnified,
    PlainSplit,
    CompressedUnified,
    CompressedSplit,
}
impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::PlainUnified => "plain_unified",
            Self::PlainSplit => "plain_split",
            Self::CompressedUnified => "compressed_unified",
            Self::CompressedSplit => "compressed_split",
        }
    }
    pub fn compressed(self) -> bool {
        matches!(self, Self::CompressedUnified | Self::CompressedSplit)
    }
    pub fn unified(self) -> bool {
        matches!(self, Self::PlainUnified | Self::CompressedUnified)
    }
    pub fn segmented(self, runtime: Runtime) -> bool {
        runtime == Runtime::Tokio && self == Self::PlainUnified
    }
}
