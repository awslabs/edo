use snafu::Snafu;

#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("io error: {source}"))]
    Io { source: std::io::Error },
    #[snafu(display("no active prompt"))]
    #[allow(dead_code)]
    NoActivePrompt,
    #[snafu(display(
        "a Ui is already entered — refusing to spawn a second EventStream (see ui::ui::ENTERED)"
    ))]
    SecondEntry,
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Error::Io { source }
    }
}
