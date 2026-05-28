//! Static HTML + markdown assets shipped inside the binary:
//! homepage, Swagger UI shell, and the agent-facing guide.

pub const HOMEPAGE: &str = include_str!("../../assets/index.html");
pub const SWAGGER_HTML: &str = include_str!("../../assets/docs.html");
pub const AGENT_MD: &str = include_str!("../../assets/agent.md");
