use std::{
    io::{Read, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context};
use clap::Parser;
use rbx_dom_weak::WeakDom;
use reqwest::blocking::{Client, Response};

use crate::web::{
    export::{CaptureResult, ExportRequest, Scope, CAPTURE_TIMEOUT},
    interface::{ServerInfoResponse, PROTOCOL_VERSION},
    util::{deserialize_msgpack, serialize_msgpack},
};

use super::build::{xml_encode_config, OutputKind};

mod binary;

const MAX_RESPONSE_SIZE: u64 = 272 * 1024 * 1024;

/// Exports live content from the Studio window connected to a Rojo+ serve port.
#[derive(Debug, Parser)]
pub struct ExportCommand {
    /// Address of the existing Rojo+ server.
    #[clap(long)]
    pub address: Option<IpAddr>,

    /// Port of the existing Rojo+ server.
    #[clap(long)]
    pub port: u16,

    /// Final file path ending in .rbxl, .rbxlx, .rbxm, or .rbxmx.
    #[clap(long, short)]
    pub output: PathBuf,

    /// Exact slash-separated instance path from game, repeatable for model exports.
    #[clap(long = "instance", conflicts_with = "selection")]
    pub instances: Vec<String>,

    /// Export Studio's current selection as a model.
    #[clap(long, conflicts_with = "instances")]
    pub selection: bool,

    /// Accept reported omissions, required for draft whole-place exports.
    #[clap(long)]
    pub allow_incomplete: bool,
}

impl ExportCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let kind = OutputKind::from_output_path(&self.output)
            .context("Output must end in .rbxl, .rbxlx, .rbxm, or .rbxmx")?;
        let scope = match kind {
            OutputKind::Rbxl | OutputKind::Rbxlx => Scope::Place,
            OutputKind::Rbxm | OutputKind::Rbxmx => Scope::Model,
        };
        let host = self.address.map_or_else(
            || "127.0.0.1".to_owned(),
            |address| match address {
                IpAddr::V4(address) => address.to_string(),
                IpAddr::V6(address) => format!("[{address}]"),
            },
        );
        let base = format!("http://{host}:{}", self.port);
        // Never follow a redirect carrying capture requests or upload capabilities elsewhere.
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(CAPTURE_TIMEOUT + Duration::from_secs(30))
            .build()?;
        let info: ServerInfoResponse = deserialize_msgpack(&read_response(
            client
                .get(format!("{base}/api/rojo"))
                .send()
                .context("Could not reach the Rojo+ serve port")?,
            1024 * 1024,
        )?)
        .context("Could not read Rojo+ server information")?;
        if info.protocol_version != PROTOCOL_VERSION {
            bail!("Live export needs matching plugin and server protocol versions. Expected {PROTOCOL_VERSION}, received {}.", info.protocol_version);
        }
        let request = ExportRequest {
            session_id: info.session_id,
            scope,
            paths: self.instances,
            selection: self.selection,
            allow_incomplete: self.allow_incomplete,
        };
        request.validate()?;
        if scope == Scope::Place && !self.allow_incomplete {
            bail!("Whole-place export is a draft, not native Studio Save As. Some service data may be omitted. Pass --allow-incomplete to accept these limitations.");
        }
        let response = client
            .post(format!("{base}/api/export"))
            .header("Content-Type", "application/msgpack")
            .body(serialize_msgpack(request)?)
            .send()
            .context("Live Studio capture failed")?;
        let capture: CaptureResult =
            deserialize_msgpack(&read_response(response, MAX_RESPONSE_SIZE)?)
                .context("Could not decode Studio's export response")?;
        let (bytes, warnings) = capture_bytes(capture, scope, kind)?;
        for warning in &warnings {
            eprintln!("Warning. {warning}");
        }
        if !warnings.is_empty() && !self.allow_incomplete {
            bail!("Studio reported export limitations. No output was written. Pass --allow-incomplete to accept them.");
        }
        write_output(&self.output, &bytes)
            .with_context(|| format!("Could not write export to \"{}\"", self.output.display()))?;
        println!(
            "Exported live Studio content to \"{}\".",
            self.output.display()
        );
        Ok(())
    }
}

fn write_output(output: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let directory = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // Staging beside the destination keeps the final replacement on one filesystem.
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    staged.persist(output).map_err(|error| error.error)?;
    Ok(())
}

fn read_response(mut response: Response, limit: u64) -> anyhow::Result<Vec<u8>> {
    let status = response.status();
    let mut bytes = Vec::new();
    response.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("The export server response exceeded the size limit.");
    }
    if !status.is_success() {
        let details = deserialize_msgpack::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("details")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
        bail!("The export server returned HTTP {status}. {details}");
    }
    Ok(bytes)
}

fn capture_bytes(
    capture: CaptureResult,
    scope: Scope,
    kind: OutputKind,
) -> anyhow::Result<(Vec<u8>, Vec<String>)> {
    let (bytes, warnings) = binary::assemble_capture(capture, scope)?;
    if matches!(kind, OutputKind::Rbxl | OutputKind::Rbxm) {
        return Ok((bytes, warnings));
    }
    binary::ensure_xml_safe(&bytes)?;
    let dom = rbx_binary::from_reader(bytes.as_slice())
        .context("Could not decode the validated capture for XML output")?;
    Ok((encode_capture(&dom, kind)?, warnings))
}

#[cfg(test)]
fn assemble_capture(
    capture: CaptureResult,
    scope: Scope,
) -> anyhow::Result<(WeakDom, Vec<String>)> {
    let (bytes, warnings) = binary::assemble_capture(capture, scope)?;
    let dom = rbx_binary::from_reader(bytes.as_slice())?;
    Ok((dom, warnings))
}

fn encode_capture(dom: &WeakDom, kind: OutputKind) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    // Both captures already have a synthetic root, unlike filesystem model builds.
    match kind {
        OutputKind::Rbxl | OutputKind::Rbxm => {
            rbx_binary::to_writer(&mut bytes, dom, dom.root().children())?
        }
        OutputKind::Rbxlx | OutputKind::Rbxmx => {
            rbx_xml::to_writer(&mut bytes, dom, dom.root().children(), xml_encode_config())?
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::export::{CaptureManifest, CaptureRoot, CaptureService};
    use rbx_dom_weak::{types::Variant, ustr, InstanceBuilder};
    use std::collections::HashMap;

    fn capture(scope: Scope) -> CaptureResult {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Folder")
                .with_name("Unsynced")
                .with_child(
                    InstanceBuilder::new("StringValue").with_property("Value", "Live content"),
                ),
        );
        let mut model_contents = Vec::new();
        rbx_binary::to_writer(&mut model_contents, &dom, dom.root().children()).unwrap();
        let service = (scope == Scope::Place).then(|| "Workspace".into());
        CaptureResult {
            model_contents,
            manifest: CaptureManifest {
                part_count: 1,
                roots: vec![CaptureRoot {
                    name: "Unsynced".into(),
                    class_name: "Folder".into(),
                    service,
                    descendant_count: 1,
                }],
                services: if scope == Scope::Place {
                    vec![CaptureService {
                        name: "Workspace".into(),
                        class_name: "Workspace".into(),
                        properties: HashMap::from([("Gravity".into(), Variant::Float32(123.0))]),
                        references: vec![],
                    }]
                } else {
                    vec![]
                },
                warnings: vec![],
                error: None,
            },
        }
    }

    fn assert_terrain_round_trip(contents: &[u8]) {
        let native = rbx_binary::from_reader(contents).unwrap();
        let terrain = native
            .descendants()
            .find(|instance| instance.class == "Terrain")
            .unwrap();
        let expected = terrain.properties.clone();
        let name = terrain.name.clone();
        let child_count = terrain.children().len();
        let descendant_count = native.descendants().count() - 2;
        for kind in [OutputKind::Rbxl, OutputKind::Rbxlx] {
            let binary = matches!(kind, OutputKind::Rbxl);
            let upload = CaptureResult {
                model_contents: contents.to_vec(),
                manifest: CaptureManifest {
                    part_count: 1,
                    roots: vec![CaptureRoot {
                        name: name.clone(),
                        class_name: "Terrain".into(),
                        service: Some("Workspace".into()),
                        descendant_count,
                    }],
                    services: vec![CaptureService {
                        name: "Workspace".into(),
                        class_name: "Workspace".into(),
                        properties: HashMap::new(),
                        references: vec![],
                    }],
                    warnings: vec![],
                    error: None,
                },
            };
            let (dom, warnings) = assemble_capture(upload, Scope::Place).unwrap();
            assert!(warnings.is_empty(), "{warnings:?}");
            let encoded = encode_capture(&dom, kind).unwrap();
            let decoded = if binary {
                rbx_binary::from_reader(encoded.as_slice()).unwrap()
            } else {
                rbx_xml::from_reader(encoded.as_slice(), rbx_xml::DecodeOptions::new()).unwrap()
            };
            let actual = decoded
                .descendants()
                .find(|instance| instance.class == "Terrain")
                .unwrap();
            for (property, value) in &expected {
                assert!(
                    actual.properties.get(property) == Some(value),
                    "Terrain property {property} changed during conversion"
                );
            }
            assert_eq!(actual.children().len(), child_count);
            let parent = decoded.get_by_ref(actual.parent()).unwrap();
            assert_eq!(parent.class.as_str(), "Workspace");
        }
    }

    #[test]
    fn terrain_voxel_storage_material_colors_water_and_children_survive_both_place_formats() {
        use rbx_dom_weak::types::{Color3, SharedString};
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let material_colors: Variant =
            serde_json::from_value(serde_json::json!({"MaterialColors": {"Grass": [10, 120, 30]}}))
                .unwrap();
        let voxel_bytes: Vec<u8> = (0..=255).collect();
        dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Terrain")
                .with_property(
                    "SmoothGrid",
                    Variant::BinaryString(voxel_bytes.clone().into()),
                )
                .with_property(
                    "PhysicsGrid",
                    Variant::BinaryString(vec![0, 255, 0, 128].into()),
                )
                .with_property(
                    "Materials",
                    Variant::SharedString(SharedString::new(voxel_bytes)),
                )
                .with_property("MaterialColors", material_colors)
                .with_property("WaterColor", Color3::new(0.25, 0.5, 0.75))
                .with_property("WaterReflectance", 0.125_f32)
                .with_property("WaterTransparency", 0.625_f32)
                .with_property("WaterWaveSize", 0.375_f32)
                .with_property("WaterWaveSpeed", 12.0_f32)
                .with_child(InstanceBuilder::new("Clouds").with_property("Cover", 0.75_f32)),
        );
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
        assert_terrain_round_trip(&bytes);
    }

    #[test]
    #[ignore = "Requires native.rbxm, terrain.rbxl, and terrain.rbxlx in ROJO_TERRAIN_TEST_DIR from a live Studio capture"]
    fn native_terrain_capture_matches_live_exports() {
        let directory = PathBuf::from(
            std::env::var_os("ROJO_TERRAIN_TEST_DIR")
                .expect("Set ROJO_TERRAIN_TEST_DIR to the live Terrain fixture directory"),
        );
        let bytes = std::fs::read(directory.join("native.rbxm")).unwrap();
        assert_terrain_round_trip(&bytes);
        let native = rbx_binary::from_reader(bytes.as_slice()).unwrap();
        let terrain = native
            .descendants()
            .find(|instance| instance.class == "Terrain")
            .unwrap();
        assert!(
            terrain.properties.contains_key(&ustr("SmoothGrid")),
            "The native fixture must include voxel storage"
        );
        for extension in ["rbxl", "rbxlx"] {
            let bytes = std::fs::read(directory.join(format!("terrain.{extension}"))).unwrap();
            let dom = if extension == "rbxl" {
                rbx_binary::from_reader(bytes.as_slice()).unwrap()
            } else {
                rbx_xml::from_reader(bytes.as_slice(), rbx_xml::DecodeOptions::new()).unwrap()
            };
            let actual = dom
                .descendants()
                .find(|instance| instance.class == "Terrain")
                .unwrap();
            for (property, value) in &terrain.properties {
                // Studio rebuilds this derived cache between independent captures. The
                // same-capture conversion above still verifies its bytes exactly.
                if property == "PhysicsGrid" {
                    continue;
                }
                assert!(
                    actual.properties.get(property) == Some(value),
                    "Terrain property {property} changed in {extension}"
                );
            }
            assert_eq!(actual.children().len(), terrain.children().len());
        }
    }

    #[test]
    fn all_four_formats_round_trip_live_content() {
        for (scope, kind) in [
            (Scope::Model, OutputKind::Rbxm),
            (Scope::Model, OutputKind::Rbxmx),
            (Scope::Place, OutputKind::Rbxl),
            (Scope::Place, OutputKind::Rbxlx),
        ] {
            let binary = matches!(kind, OutputKind::Rbxm | OutputKind::Rbxl);
            let (bytes, warnings) = capture_bytes(capture(scope), scope, kind).unwrap();
            assert!(warnings.is_empty());
            let decoded = if binary {
                rbx_binary::from_reader(bytes.as_slice()).unwrap()
            } else {
                rbx_xml::from_reader(bytes.as_slice(), rbx_xml::DecodeOptions::new()).unwrap()
            };
            let root = decoded.get_by_ref(decoded.root().children()[0]).unwrap();
            let folder = if scope == Scope::Place {
                assert_eq!(root.class.as_str(), "Workspace");
                assert_eq!(
                    root.properties.get(&ustr("Gravity")),
                    Some(&Variant::Float32(123.0))
                );
                decoded.get_by_ref(root.children()[0]).unwrap()
            } else {
                root
            };
            assert_eq!(folder.name, "Unsynced");
            let value = decoded.get_by_ref(folder.children()[0]).unwrap();
            assert_eq!(
                value.properties.get(&ustr("Value")),
                Some(&Variant::String("Live content".into()))
            );
        }
    }

    #[test]
    fn cross_root_references_and_script_source_survive_conversion() {
        for kind in [OutputKind::Rbxm, OutputKind::Rbxmx] {
            let binary = matches!(kind, OutputKind::Rbxm);
            let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
            let target = dom.insert(
                dom.root_ref(),
                InstanceBuilder::new("ModuleScript")
                    .with_name("LiveScript")
                    .with_property("Source", "return 42"),
            );
            dom.insert(
                dom.root_ref(),
                InstanceBuilder::new("ObjectValue")
                    .with_name("Link")
                    .with_property("Value", target),
            );
            let mut model_contents = Vec::new();
            rbx_binary::to_writer(&mut model_contents, &dom, dom.root().children()).unwrap();
            let mut upload = capture(Scope::Model);
            upload.model_contents = model_contents;
            upload.manifest.roots = vec![
                CaptureRoot {
                    name: "LiveScript".into(),
                    class_name: "ModuleScript".into(),
                    service: None,
                    descendant_count: 0,
                },
                CaptureRoot {
                    name: "Link".into(),
                    class_name: "ObjectValue".into(),
                    service: None,
                    descendant_count: 0,
                },
            ];
            let (dom, warnings) = assemble_capture(upload, Scope::Model).unwrap();
            assert!(warnings.is_empty());
            let bytes = encode_capture(&dom, kind).unwrap();
            let decoded = if binary {
                rbx_binary::from_reader(bytes.as_slice()).unwrap()
            } else {
                rbx_xml::from_reader(bytes.as_slice(), rbx_xml::DecodeOptions::new()).unwrap()
            };
            let children = decoded.root().children();
            assert_eq!(
                decoded.get_by_ref(children[0]).unwrap().properties[&ustr("Source")],
                Variant::String("return 42".into())
            );
            assert_eq!(
                decoded.get_by_ref(children[1]).unwrap().properties[&ustr("Value")],
                Variant::Ref(children[0])
            );
        }
    }

    #[test]
    fn omissions_are_reported_and_misaligned_roots_fail() {
        let mut upload = capture(Scope::Model);
        upload.manifest.roots[0].descendant_count = 2;
        assert_eq!(assemble_capture(upload, Scope::Model).unwrap().1.len(), 1);
        let mut upload = capture(Scope::Model);
        upload.manifest.roots[0].name = "Wrong".into();
        assert!(assemble_capture(upload, Scope::Model).is_err());
        let mut upload = capture(Scope::Model);
        upload.manifest.roots.clear();
        assert!(assemble_capture(upload, Scope::Model).is_err());
    }

    #[test]
    fn place_captures_preserve_nonservice_top_level_instances() {
        let mut upload = capture(Scope::Model);
        upload.manifest.services.push(CaptureService {
            name: "Workspace".into(),
            class_name: "Workspace".into(),
            properties: HashMap::new(),
            references: vec![],
        });
        let (dom, warnings) = assemble_capture(upload, Scope::Place).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(dom.root().children().len(), 2);
        assert_eq!(
            dom.get_by_ref(dom.root().children()[0]).unwrap().name,
            "Unsynced"
        );
    }

    #[test]
    fn public_material_override_names_survive_binary_and_xml_place_output() {
        let expected = [
            ("AsphaltName", "AuthoredAsphalt"),
            ("GrassName", ""),
            ("WoodName", "AuthoredWood"),
        ];
        for kind in [OutputKind::Rbxl, OutputKind::Rbxlx] {
            let mut upload = capture(Scope::Place);
            upload.manifest.services.push(CaptureService {
                name: "MaterialService".into(),
                class_name: "MaterialService".into(),
                properties: expected
                    .iter()
                    .map(|(name, value)| (name.to_string(), Variant::String(value.to_string())))
                    .collect(),
                references: vec![],
            });
            let (bytes, warnings) = capture_bytes(upload, Scope::Place, kind).unwrap();
            assert!(warnings.is_empty(), "{warnings:?}");
            let decoded = if matches!(kind, OutputKind::Rbxl) {
                rbx_binary::from_reader(bytes.as_slice()).unwrap()
            } else {
                rbx_xml::from_reader(bytes.as_slice(), rbx_xml::DecodeOptions::new()).unwrap()
            };
            let service = decoded
                .descendants()
                .find(|instance| instance.class == "MaterialService")
                .unwrap();
            for (name, value) in expected {
                assert_eq!(
                    service.properties.get(&ustr(name)),
                    Some(&Variant::String(value.into()))
                );
            }
        }
    }

    #[test]
    fn new_service_enums_preserve_numeric_values_in_binary_and_xml_place_output() {
        use rbx_dom_weak::types::Enum;
        for value in 0..=2 {
            for kind in [OutputKind::Rbxl, OutputKind::Rbxlx] {
                let expected = Variant::Enum(Enum::from_u32(value));
                let mut upload = capture(Scope::Place);
                upload.manifest.services[0]
                    .properties
                    .insert("NextGenerationPhysicsSolver".into(), expected.clone());
                upload.manifest.services.push(CaptureService {
                    name: "Lighting".into(),
                    class_name: "Lighting".into(),
                    properties: HashMap::from([("LightAttenuationMode".into(), expected.clone())]),
                    references: vec![],
                });
                let (bytes, warnings) = capture_bytes(upload, Scope::Place, kind).unwrap();
                assert!(warnings.is_empty(), "{warnings:?}");
                let decoded = if matches!(kind, OutputKind::Rbxl) {
                    rbx_binary::from_reader(bytes.as_slice()).unwrap()
                } else {
                    rbx_xml::from_reader(
                        bytes.as_slice(),
                        rbx_xml::DecodeOptions::new()
                            .property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown),
                    )
                    .unwrap()
                };
                for (class, property) in [
                    ("Workspace", "NextGenerationPhysicsSolver"),
                    ("Lighting", "LightAttenuationMode"),
                ] {
                    let service = decoded
                        .descendants()
                        .find(|instance| instance.class == class)
                        .unwrap();
                    assert_eq!(service.properties.get(&ustr(property)), Some(&expected));
                }
            }
        }
    }

    #[test]
    fn captured_current_camera_identity_survives_binary_and_xml_place_output() {
        use crate::web::export::{CaptureReference, CaptureReferenceTarget};
        for kind in [OutputKind::Rbxl, OutputKind::Rbxlx] {
            let mut source = WeakDom::new(InstanceBuilder::new("DataModel"));
            for field_of_view in [30.0_f32, 80.0_f32] {
                source.insert(
                    source.root_ref(),
                    InstanceBuilder::new("Camera")
                        .with_name("Same")
                        .with_property("FieldOfView", field_of_view),
                );
            }
            let mut upload = capture(Scope::Place);
            upload.model_contents.clear();
            rbx_binary::to_writer(
                &mut upload.model_contents,
                &source,
                source.root().children(),
            )
            .unwrap();
            upload.manifest.roots = (0..2)
                .map(|_| CaptureRoot {
                    name: "Same".into(),
                    class_name: "Camera".into(),
                    service: Some("Workspace".into()),
                    descendant_count: 0,
                })
                .collect();
            upload.manifest.services[0]
                .references
                .push(CaptureReference {
                    property: "CurrentCamera".into(),
                    target: CaptureReferenceTarget::Root { index: 1 },
                });
            let (bytes, warnings) = capture_bytes(upload, Scope::Place, kind).unwrap();
            assert!(warnings.is_empty(), "{warnings:?}");
            let decoded = if matches!(kind, OutputKind::Rbxl) {
                rbx_binary::from_reader(bytes.as_slice()).unwrap()
            } else {
                rbx_xml::from_reader(bytes.as_slice(), rbx_xml::DecodeOptions::new()).unwrap()
            };
            let workspace = decoded
                .descendants()
                .find(|instance| instance.class == "Workspace")
                .unwrap();
            let Variant::Ref(target) = workspace.properties[&ustr("CurrentCamera")] else {
                panic!("Missing camera reference");
            };
            let camera = decoded.get_by_ref(target).unwrap();
            assert_eq!(camera.name, "Same");
            assert_eq!(
                camera.properties.get(&ustr("FieldOfView")),
                Some(&Variant::Float32(80.0))
            );
        }
    }

    #[test]
    fn unknown_services_and_corrupt_models_fail() {
        let mut upload = capture(Scope::Place);
        upload.manifest.services.clear();
        assert!(assemble_capture(upload, Scope::Place).is_err());
        let mut upload = capture(Scope::Model);
        upload.model_contents = vec![1, 2, 3];
        assert!(assemble_capture(upload, Scope::Model).is_err());
    }

    #[test]
    fn service_references_are_reported_instead_of_written_with_unmapped_ids() {
        let mut upload = capture(Scope::Place);
        upload.manifest.services[0].properties.insert(
            "CurrentCamera".into(),
            Variant::Ref(rbx_dom_weak::types::Ref::new()),
        );
        let (dom, warnings) = assemble_capture(upload, Scope::Place).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Workspace.CurrentCamera"));
        let workspace = dom
            .descendants()
            .find(|instance| instance.class == "Workspace")
            .unwrap();
        assert!(!workspace.properties.contains_key(&ustr("CurrentCamera")));
    }

    #[test]
    fn output_replacement_cleans_staging_on_success_and_failure() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("out.rbxm");
        std::fs::write(&output, b"previous export").unwrap();
        write_output(&output, b"new export").unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"new export");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        let blocked = directory.path().join("directory.rbxm");
        std::fs::create_dir(&blocked).unwrap();
        assert!(write_output(&blocked, b"replacement").is_err());
        assert!(blocked.is_dir());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
        assert_eq!(std::fs::read(&output).unwrap(), b"new export");
    }

    #[test]
    fn cli_requires_connection_and_output_arguments() {
        assert!(ExportCommand::try_parse_from(["export", "--output", "out.rbxm"]).is_err());
        assert!(ExportCommand::try_parse_from(["export", "--port", "34872"]).is_err());
        assert!(ExportCommand::try_parse_from([
            "export",
            "--port",
            "34872",
            "--output",
            "out.rbxm",
            "--instance",
            "Workspace/Foo",
            "--selection"
        ])
        .is_err());
    }
}
