use std::{fmt, str::FromStr};

/// MCP logging severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum McpLogLevel {
    /// Debug-level diagnostics.
    Debug,
    /// Informational messages.
    Info,
    /// Normal but significant conditions.
    Notice,
    /// Warning conditions.
    Warning,
    /// Error conditions.
    Error,
    /// Critical conditions.
    Critical,
    /// Immediate action required.
    Alert,
    /// System is unusable.
    Emergency,
}

impl McpLogLevel {
    pub(in crate::mcp) const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Notice => "notice",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Critical => "critical",
            Self::Alert => "alert",
            Self::Emergency => "emergency",
        }
    }
}

impl fmt::Display for McpLogLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for McpLogLevel {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "notice" => Ok(Self::Notice),
            "warning" => Ok(Self::Warning),
            "error" => Ok(Self::Error),
            "critical" => Ok(Self::Critical),
            "alert" => Ok(Self::Alert),
            "emergency" => Ok(Self::Emergency),
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::McpLogLevel;

    #[test]
    fn parses_mcp_log_levels() {
        assert_eq!("debug".parse(), Ok(McpLogLevel::Debug));
        assert_eq!("info".parse(), Ok(McpLogLevel::Info));
        assert_eq!("notice".parse(), Ok(McpLogLevel::Notice));
        assert_eq!("warning".parse(), Ok(McpLogLevel::Warning));
        assert_eq!("error".parse(), Ok(McpLogLevel::Error));
        assert_eq!("critical".parse(), Ok(McpLogLevel::Critical));
        assert_eq!("alert".parse(), Ok(McpLogLevel::Alert));
        assert_eq!("emergency".parse(), Ok(McpLogLevel::Emergency));
        assert!("warn".parse::<McpLogLevel>().is_err());
    }
}
