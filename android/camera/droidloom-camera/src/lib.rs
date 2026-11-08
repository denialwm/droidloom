//! The pieces of Droidloom's camera HAL that need no Android: camera
//! metadata, what each braun camera is described as, request settings and
//! 3A, braun's session protocol, preview conversion and the JPEG's
//! orientation. The service binary in `droidloom-camera-service` puts them
//! behind `ICameraProvider`.

pub mod braun;
pub mod characteristics;
pub mod control;
pub mod exif;
pub mod frame;
pub mod metadata;
pub mod tags;
