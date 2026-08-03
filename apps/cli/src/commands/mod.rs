//! One module per verb family. Each returns `(json data, human text)` so the
//! output layer owns the envelope and the verbs own only their content.

pub mod agent_context;
pub mod sessions;
pub mod status;
