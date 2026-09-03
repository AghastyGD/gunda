/// General resource category selected during inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Unknown,
    File,
    Hls,
    Dash,
}

/// Generic information discovered about a remote resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceDescriptor {
    kind: ResourceKind,
    display_name: Option<String>,
    content_type: Option<String>,
}

impl ResourceDescriptor {
    #[must_use]
    pub fn new(
        kind: ResourceKind,
        display_name: Option<String>,
        content_type: Option<String>,
    ) -> Self {
        Self {
            kind,
            display_name,
            content_type,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ResourceKind {
        self.kind
    }

    #[must_use]
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::{ResourceDescriptor, ResourceKind};

    #[test]
    fn resource_descriptor_preserves_generic_metadata() {
        let resource = ResourceDescriptor::new(
            ResourceKind::File,
            Some("image.iso".to_owned()),
            Some("application/octet-stream".to_owned()),
        );

        assert_eq!(resource.kind(), ResourceKind::File);
        assert_eq!(resource.display_name(), Some("image.iso"));
        assert_eq!(resource.content_type(), Some("application/octet-stream"));
    }
}
