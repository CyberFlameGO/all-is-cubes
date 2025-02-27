use std::io;
use std::time::Duration;

use gltf_json::validation::Checked::Valid;
use gltf_json::Index;

use super::buffer::create_buffer_and_accessor;
use super::glue::{convert_quaternion, push_and_return_index};
use super::GltfWriter;

#[derive(Debug)]
pub(crate) struct FrameState {
    /// The set of nodes that should be visible in this frame.
    /// This information will be used to assemble a glTF animation.
    ///
    /// glTF animation doesn't support adding/removing objects explicitly, but
    /// setting the scale to zero is explicitly noted in the specification, and so
    /// that is the animation that is generated.
    pub visible_nodes: Vec<Index<gltf_json::Node>>,

    // The camera's state in this frame.
    pub camera_transform: all_is_cubes::camera::ViewTransform,
}

pub(crate) fn add_camera_animation(
    writer: &mut GltfWriter,
    camera_node_index: Index<gltf_json::Node>,
    frame_pace: Duration,
) -> io::Result<()> {
    let mut animation_channels = Vec::new();
    let mut animation_samplers = Vec::new();

    let time_accessor = create_buffer_and_accessor(
        &mut writer.root,
        &mut writer.buffer_dest,
        "camera animation time".into(),
        "camera-time",
        writer
            .frame_states
            .iter()
            .enumerate()
            .map(|(i, _)| [frame_pace.as_secs_f32() * i as f32]),
    )?;

    // Translation
    animation_channels.push(gltf_json::animation::Channel {
        sampler: push_and_return_index(
            &mut animation_samplers,
            gltf_json::animation::Sampler {
                input: time_accessor,
                interpolation: Valid(gltf_json::animation::Interpolation::Linear),
                output: create_buffer_and_accessor(
                    &mut writer.root,
                    &mut writer.buffer_dest,
                    "camera animation translation".into(),
                    "camera-pos",
                    writer
                        .frame_states
                        .iter()
                        .map(|frame| frame.camera_transform.disp.cast::<f32>().unwrap().into()),
                )?,
                extensions: Default::default(),
                extras: Default::default(),
            },
        ),
        target: gltf_json::animation::Target {
            node: camera_node_index,
            path: Valid(gltf_json::animation::Property::Translation),
            extensions: Default::default(),
            extras: Default::default(),
        },
        extensions: Default::default(),
        extras: Default::default(),
    });
    // Rotation
    animation_channels.push(gltf_json::animation::Channel {
        sampler: push_and_return_index(
            &mut animation_samplers,
            gltf_json::animation::Sampler {
                input: time_accessor,
                interpolation: Valid(gltf_json::animation::Interpolation::Linear),
                output: create_buffer_and_accessor(
                    &mut writer.root,
                    &mut writer.buffer_dest,
                    "camera animation rotation".into(),
                    "camera-rot",
                    writer
                        .frame_states
                        .iter()
                        .map(|frame| convert_quaternion(frame.camera_transform.rot).0),
                )?,
                extensions: Default::default(),
                extras: Default::default(),
            },
        ),
        target: gltf_json::animation::Target {
            node: camera_node_index,
            path: Valid(gltf_json::animation::Property::Rotation),
            extensions: Default::default(),
            extras: Default::default(),
        },
        extensions: Default::default(),
        extras: Default::default(),
    });

    push_and_return_index(
        &mut writer.root.animations,
        gltf_json::Animation {
            name: Some("camera movement".into()),
            channels: animation_channels,
            samplers: animation_samplers,
            extensions: Default::default(),
            extras: Default::default(),
        },
    );

    Ok(())
}
