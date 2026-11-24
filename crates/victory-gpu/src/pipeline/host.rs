//! Host pipelines for title pipelines (decision 0007, step 3): the
//! title's graphics state with the translated shaders, under a root
//! signature of our own that the translated shaders' bindings define
//! (`victory_shader::emit`): per stage one block of root constants for the
//! user data, root views for the title's root views, static samplers for
//! the immediate samplers, and directly indexed heaps.
//!
//! A pipeline with a geometry stage runs the ES half of its vertex
//! container as the vertex shader and the GS half of its geometry
//! container, each with its own bindings as the other stages have.

use std::ffi::{CStr, CString};
use std::sync::Arc;

use victory_shader::emit::{ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;

use super::{Program, Shader, Translated};
use crate::host::host;

/// A stream's graphics state, owned (the title's stream lives only for the
/// creation call). Subobjects the stream leaves out keep D3D12's defaults.
pub(crate) struct GraphicsState {
    blend: D3D12_BLEND_DESC,
    sample_mask: u32,
    rasterizer: D3D12_RASTERIZER_DESC,
    depth: D3D12_DEPTH_STENCIL_DESC,
    /// Input elements, their names pointing into `names`.
    inputs: Vec<D3D12_INPUT_ELEMENT_DESC>,
    names: Vec<CString>,
    strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE,
    pub(crate) topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE,
    pub(crate) targets: D3D12_RT_FORMAT_ARRAY,
    depth_format: DXGI_FORMAT,
    samples: DXGI_SAMPLE_DESC,
}

impl Default for GraphicsState {
    fn default() -> Self {
        let target = D3D12_RENDER_TARGET_BLEND_DESC {
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_ZERO,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_ZERO,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
            ..Default::default()
        };
        let keep = D3D12_DEPTH_STENCILOP_DESC {
            StencilFailOp: D3D12_STENCIL_OP_KEEP,
            StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
            StencilPassOp: D3D12_STENCIL_OP_KEEP,
            StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        };
        GraphicsState {
            blend: D3D12_BLEND_DESC {
                RenderTarget: [target; 8],
                ..Default::default()
            },
            sample_mask: u32::MAX,
            rasterizer: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_BACK,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            depth: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: true.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                DepthFunc: D3D12_COMPARISON_FUNC_LESS,
                StencilReadMask: 0xff,
                StencilWriteMask: 0xff,
                FrontFace: keep,
                BackFace: keep,
                ..Default::default()
            },
            inputs: Vec::new(),
            names: Vec::new(),
            strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED,
            topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE_UNDEFINED,
            targets: D3D12_RT_FORMAT_ARRAY::default(),
            depth_format: DXGI_FORMAT_UNKNOWN,
            samples: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
        }
    }
}

impl GraphicsState {
    /// Takes in one subobject of a stream: its type, then its data at +4
    /// (+8 for those holding pointers).
    ///
    /// # Safety
    /// `at` points at a whole subobject of type `kind`.
    pub(crate) unsafe fn read(&mut self, kind: u32, at: *const u8) {
        // SAFETY: the caller passes a whole subobject.
        unsafe {
            let data = at.add(4);
            match kind {
                8 => self.blend = data.cast::<D3D12_BLEND_DESC>().read_unaligned(),
                9 => self.sample_mask = data.cast::<u32>().read_unaligned(),
                10 => self.rasterizer = data.cast::<D3D12_RASTERIZER_DESC>().read_unaligned(),
                11 => self.depth = data.cast::<D3D12_DEPTH_STENCIL_DESC>().read_unaligned(),
                21 => {
                    // shortcut: the depth bounds test is dropped (the host
                    // pipeline uses the plain description); bring it back
                    // with a stream if a title enables it.
                    let d = data.cast::<D3D12_DEPTH_STENCIL_DESC1>().read_unaligned();
                    self.depth = D3D12_DEPTH_STENCIL_DESC {
                        DepthEnable: d.DepthEnable,
                        DepthWriteMask: d.DepthWriteMask,
                        DepthFunc: d.DepthFunc,
                        StencilEnable: d.StencilEnable,
                        StencilReadMask: d.StencilReadMask,
                        StencilWriteMask: d.StencilWriteMask,
                        FrontFace: d.FrontFace,
                        BackFace: d.BackFace,
                    };
                }
                12 => {
                    let layout = at.add(8).cast::<D3D12_INPUT_LAYOUT_DESC>().read_unaligned();
                    if layout.pInputElementDescs.is_null() {
                        return;
                    }
                    let elements = std::slice::from_raw_parts(
                        layout.pInputElementDescs,
                        layout.NumElements as usize,
                    );
                    self.names = elements
                        .iter()
                        .map(|e| CStr::from_ptr(e.SemanticName.0.cast()).to_owned())
                        .collect();
                    self.inputs = elements
                        .iter()
                        .zip(&self.names)
                        .map(|(e, name)| D3D12_INPUT_ELEMENT_DESC {
                            SemanticName: PCSTR(name.as_ptr().cast()),
                            Format: DXGI_FORMAT(
                                crate::resource::pc_format(e.Format.0 as u32) as i32
                            ),
                            ..*e
                        })
                        .collect();
                }
                13 => {
                    self.strip_cut =
                        D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(data.cast::<i32>().read_unaligned())
                }
                14 => {
                    self.topology =
                        D3D12_PRIMITIVE_TOPOLOGY_TYPE(data.cast::<i32>().read_unaligned())
                }
                15 => {
                    self.targets = data.cast::<D3D12_RT_FORMAT_ARRAY>().read_unaligned();
                    for f in &mut self.targets.RTFormats {
                        f.0 = crate::resource::pc_format(f.0 as u32) as i32;
                    }
                }
                16 => {
                    let f = data.cast::<u32>().read_unaligned();
                    self.depth_format = DXGI_FORMAT(crate::resource::pc_format(f) as i32);
                }
                17 => self.samples = data.cast::<DXGI_SAMPLE_DESC>().read_unaligned(),
                _ => {}
            }
        }
    }

    /// Everything the state holds, as text: part of a pipeline's name in
    /// the library and of the key a title pipeline is started early by.
    pub(crate) fn describe(&self) -> String {
        let inputs: Vec<String> = self
            .inputs
            .iter()
            .zip(&self.names)
            .map(|(e, name)| {
                format!(
                    "{name:?} {} {} {} {} {} {}",
                    e.SemanticIndex, e.Format.0, e.InputSlot, e.AlignedByteOffset, e.InputSlotClass.0, e.InstanceDataStepRate
                )
            })
            .collect();
        format!(
            "{:?} {} {:?} {:?} {inputs:?} {} {} {:?} {} {:?}",
            self.blend,
            self.sample_mask,
            self.rasterizer,
            self.depth,
            self.strip_cut.0,
            self.topology.0,
            self.targets,
            self.depth_format.0,
            self.samples
        )
    }

    /// The vertex buffer slots the input layout reads.
    fn slots(&self) -> u32 {
        self.inputs
            .iter()
            .fold(0, |m, e| m | 1 << e.InputSlot.min(31))
    }

    /// The input elements with their byte offsets worked out.
    fn elements(&self) -> Vec<Input> {
        let mut next = [0u32; 32];
        self.inputs
            .iter()
            .map(|e| {
                let slot = e.InputSlot.min(31) as usize;
                let format = e.Format.0 as u32;
                let offset = if e.AlignedByteOffset == D3D12_APPEND_ALIGNED_ELEMENT {
                    next[slot]
                } else {
                    e.AlignedByteOffset
                };
                let bytes = crate::resource::element(format).map_or(0, |(b, _)| b as u32);
                next[slot] = offset + bytes;
                Input {
                    slot: slot as u32,
                    offset,
                    format,
                    per_instance: e.InputSlotClass == D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                }
            })
            .collect()
    }
}

/// One input element: vertex buffer slot, byte offset, format.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Input {
    pub(crate) slot: u32,
    pub(crate) offset: u32,
    pub(crate) format: u32,
    pub(crate) per_instance: bool,
}

/// The console's own primitive topology types.
const RECTANGLES: i32 = 19;
const QUADS: i32 = 32;

/// One stage of a host pipeline and where its bindings go.
pub(crate) struct Stage {
    pub(crate) shader: Arc<Shader>,
    /// Which of its container's programs runs.
    pub(crate) program: Program,
    /// The root parameter of its user data, and its size in dwords.
    pub(crate) user_data: Option<(u32, u32)>,
    /// Root parameters of its root views: (title root parameter, ours).
    pub(crate) root_cbvs: Vec<(u8, u32)>,
    pub(crate) root_srvs: Vec<(u8, u32)>,
}

impl Stage {
    pub(crate) fn translated(&self) -> &Translated {
        self.shader
            .translated_as(self.program)
            .expect("host pipelines are made from translated shaders")
    }
}

pub(crate) struct HostPipeline {
    /// A compute pipeline (else graphics).
    pub(crate) compute: bool,
    /// A mesh pipeline (graphics without vertex input).
    pub(crate) mesh: bool,
    pub(crate) pso: ID3D12PipelineState,
    pub(crate) root: ID3D12RootSignature,
    /// Vertex (or mesh), geometry if any, then pixel stage; or the compute
    /// stage.
    pub(crate) stages: Vec<Stage>,
    /// The vertex buffer slots it reads (bit per slot).
    pub(crate) slots: u32,
    pub(crate) inputs: Vec<Input>,
    /// Its shaders, for reports of slow submissions.
    pub(crate) name: String,
}

/// Logs a pipeline that gets no host pipeline, once per reason.
fn refuse(reason: &'static str, shaders: &[Arc<Shader>]) -> Option<HostPipeline> {
    static SEEN: std::sync::Mutex<Vec<&str>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&reason) {
        seen.push(reason);
        tracing::warn!(?shaders, "no host pipeline: {reason} (logged once)");
    }
    None
}

impl HostPipeline {
    /// The legacy geometry stage accepts triangles without adjacency.
    pub(crate) fn supports_input_topology(
        &self,
        topology: windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY,
    ) -> bool {
        use windows::Win32::Graphics::Direct3D::{
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
        };
        matches!(
            topology,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST | D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP
        ) || self.stages.iter().all(|stage| stage.program != Program::Gs)
    }

    /// A host pipeline for vertex (or mesh), optionally geometry, and pixel
    /// shaders with the graphics state `g`, or for one compute shader.
    pub(crate) fn new(shaders: &[Arc<Shader>], g: Option<&GraphicsState>) -> Option<HostPipeline> {
        let find = |stage| shaders.iter().find(|s| s.stage == stage);
        let compute = match shaders {
            [cs] if cs.stage == "compute" => Some(cs),
            _ => None,
        };
        if compute.is_none()
            && shaders
                .iter()
                .any(|s| !matches!(s.stage, "vertex" | "pixel" | "mesh" | "geometry"))
        {
            return refuse(
                "only vertex, mesh, geometry, pixel and compute stages are translated",
                shaders,
            );
        }
        let mesh = find("mesh");
        let geometry = find("geometry");
        let stages = if let Some(cs) = compute {
            vec![(cs, D3D12_SHADER_VISIBILITY_ALL, Program::Main)]
        } else {
            let first = match (mesh, find("vertex"), geometry) {
                (Some(_), _, Some(_)) => {
                    return refuse("a mesh pipeline with a geometry stage", shaders);
                }
                (Some(ms), _, None) => (ms, D3D12_SHADER_VISIBILITY_MESH, Program::Main),
                (None, Some(vs), gs) => (
                    vs,
                    D3D12_SHADER_VISIBILITY_VERTEX,
                    Program::of(vs.stage, gs.is_some()),
                ),
                (None, None, _) => return refuse("no vertex or mesh shader", shaders),
            };
            let mut stages = vec![first];
            stages.extend(geometry.map(|gs| (gs, D3D12_SHADER_VISIBILITY_GEOMETRY, Program::Gs)));
            stages
                .extend(find("pixel").map(|ps| (ps, D3D12_SHADER_VISIBILITY_PIXEL, Program::Main)));
            stages
        };
        if mesh.is_some() && !host()?.mesh_shaders() {
            return refuse("the host GPU has no mesh shaders (or no shader model 6.6)", shaders);
        }
        for &(s, _, program) in &stages {
            s.translated_as(program)?;
        }
        let host = host()?;

        // The root signature the translated shaders' bindings define.
        let mut params: Vec<D3D12_ROOT_PARAMETER1> = Vec::new();
        let mut samplers = Vec::new();
        let mut bound = Vec::new();
        for &(shader, visibility, program) in &stages {
            let t = &shader.translated_as(program)?.host;
            let mut stage = Stage {
                shader: shader.clone(),
                program,
                user_data: None,
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
            };
            let dwords = t.user_data_dwords();
            if dwords > 0 {
                stage.user_data = Some((params.len() as u32, dwords));
                params.push(D3D12_ROOT_PARAMETER1 {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: D3D12_ROOT_PARAMETER1_0 {
                        Constants: D3D12_ROOT_CONSTANTS {
                            ShaderRegister: 0,
                            RegisterSpace: USER_DATA_SPACE,
                            Num32BitValues: dwords,
                        },
                    },
                    ShaderVisibility: visibility,
                });
            }
            for (slots, kind, out) in [
                (
                    &t.root_cbvs,
                    D3D12_ROOT_PARAMETER_TYPE_CBV,
                    &mut stage.root_cbvs,
                ),
                (
                    &t.root_srvs,
                    D3D12_ROOT_PARAMETER_TYPE_SRV,
                    &mut stage.root_srvs,
                ),
            ] {
                for &slot in slots {
                    out.push((slot, params.len() as u32));
                    params.push(D3D12_ROOT_PARAMETER1 {
                        ParameterType: kind,
                        Anonymous: D3D12_ROOT_PARAMETER1_0 {
                            Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                                ShaderRegister: u32::from(slot),
                                RegisterSpace: ROOT_CBV_SPACE,
                                Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                            },
                        },
                        ShaderVisibility: visibility,
                    });
                }
            }
            for (n, s) in t.static_samplers.iter().enumerate() {
                samplers.push(D3D12_STATIC_SAMPLER_DESC {
                    Filter: D3D12_FILTER(s.filter as i32),
                    AddressU: D3D12_TEXTURE_ADDRESS_MODE(s.address[0] as i32),
                    AddressV: D3D12_TEXTURE_ADDRESS_MODE(s.address[1] as i32),
                    AddressW: D3D12_TEXTURE_ADDRESS_MODE(s.address[2] as i32),
                    MipLODBias: s.mip_lod_bias,
                    MaxAnisotropy: s.max_anisotropy,
                    ComparisonFunc: D3D12_COMPARISON_FUNC(s.comparison as i32),
                    BorderColor: D3D12_STATIC_BORDER_COLOR(s.border as i32),
                    MinLOD: s.min_lod,
                    MaxLOD: s.max_lod,
                    ShaderRegister: n as u32,
                    RegisterSpace: SAMPLER_SPACE,
                    ShaderVisibility: visibility,
                });
            }
            bound.push(stage);
        }
        let (root, root_blob) = match root_signature(&host.device, &params, &samplers, compute.is_some()) {
            Ok(root) => root,
            Err(e) => {
                tracing::warn!(error = %e, ?shaders, "host root signature refused");
                return None;
            }
        };
        // The bytecode of the stage with this visibility (none if absent).
        let code = |visibility: D3D12_SHADER_VISIBILITY| {
            let stage = stages.iter().find(|s| s.1 == visibility);
            stage
                .and_then(|&(s, _, program)| s.translated_as(program))
                .map_or(D3D12_SHADER_BYTECODE::default(), |t| {
                    D3D12_SHADER_BYTECODE {
                        pShaderBytecode: t.host.dxil.as_ptr().cast(),
                        BytecodeLength: t.host.dxil.len(),
                    }
                })
        };
        if compute.is_some() {
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                // SAFETY: a borrowed pointer the description never releases.
                pRootSignature: unsafe { std::mem::transmute_copy(&root) },
                CS: code(D3D12_SHADER_VISIBILITY_ALL),
                ..Default::default()
            };
            let name = super::library::name(
                "compute",
                &root_blob,
                &[&code_bytes(&code(D3D12_SHADER_VISIBILITY_ALL))],
                "",
            );
            let made = super::library::pipeline(
                &name,
                // SAFETY: the description points at live data.
                |library, name| unsafe { library.LoadComputePipeline(name, &desc) },
                // SAFETY: as above.
                || unsafe { host.device.CreateComputePipelineState(&desc) },
            );
            let pso = match made {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host pipeline refused");
                    host.log_messages();
                    return None;
                }
            };
            tracing::info!(?shaders, "host pipeline");
            crate::host::name(&pso, || format!("{shaders:?}"));
            return Some(HostPipeline {
                compute: true,
                mesh: false,
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                slots: 0,
                inputs: Vec::new(),
            });
        }
        let Some(g) = g else {
            return refuse("graphics state not kept (plain description)", shaders);
        };
        let state = g.describe();
        let (vs, ps) = (
            code(if mesh.is_some() {
                D3D12_SHADER_VISIBILITY_MESH
            } else {
                D3D12_SHADER_VISIBILITY_VERTEX
            }),
            code(D3D12_SHADER_VISIBILITY_PIXEL),
        );
        if mesh.is_some() {
            let name = super::library::name(
                "mesh",
                &root_blob,
                &[&code_bytes(&vs), &code_bytes(&ps)],
                &state,
            );
            let pso = match mesh_pipeline(&host.device, &root, vs, ps, g, &name) {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host mesh pipeline refused");
                    host.log_messages();
                    return None;
                }
            };
            tracing::info!(?shaders, "host mesh pipeline");
            crate::host::name(&pso, || format!("{shaders:?}"));
            return Some(HostPipeline {
                compute: false,
                mesh: true,
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                slots: 0,
                inputs: Vec::new(),
            });
        }

        let gs = code(D3D12_SHADER_VISIBILITY_GEOMETRY);
        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the description never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            VS: vs,
            PS: ps,
            GS: gs,
            BlendState: g.blend,
            SampleMask: g.sample_mask,
            RasterizerState: g.rasterizer,
            DepthStencilState: g.depth,
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: g.inputs.as_ptr(),
                NumElements: g.inputs.len() as u32,
            },
            IBStripCutValue: g.strip_cut,
            // The console's rectangle and quad types draw as triangles on
            // the host (see `draw`).
            // shortcut: a geometry stage is always given triangles (the
            // translated GS declares triangle input); a title drawing
            // another primitive through one would need the GS translated
            // for that primitive.
            PrimitiveTopologyType: match g.topology.0 {
                _ if geometry.is_some() => D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                RECTANGLES | QUADS => D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                _ => g.topology,
            },
            NumRenderTargets: g.targets.NumRenderTargets,
            RTVFormats: g.targets.RTFormats,
            DSVFormat: g.depth_format,
            SampleDesc: g.samples,
            ..Default::default()
        };
        // Pipelines without a geometry stage keep the names they had.
        let name = if geometry.is_some() {
            super::library::name(
                "geometry",
                &root_blob,
                &[&code_bytes(&vs), &code_bytes(&gs), &code_bytes(&ps)],
                &state,
            )
        } else {
            super::library::name(
                "graphics",
                &root_blob,
                &[&code_bytes(&vs), &code_bytes(&ps)],
                &state,
            )
        };
        let made = super::library::pipeline(
            &name,
            // SAFETY: the description points at live data.
            |library, name| unsafe { library.LoadGraphicsPipeline(name, &desc) },
            // SAFETY: as above.
            || unsafe { host.device.CreateGraphicsPipelineState(&desc) },
        );
        let pso = match made {
            Ok(pso) => pso,
            Err(e) => {
                tracing::warn!(error = %e, ?shaders, "host pipeline refused");
                host.log_messages();
                return None;
            }
        };
        tracing::info!(?shaders, "host pipeline");
        crate::host::name(&pso, || format!("{shaders:?}"));
        tracing::debug!(
            ?shaders,
            depth = ?g.depth,
            dsv = g.depth_format.0,
            targets = ?&g.targets.RTFormats[..g.targets.NumRenderTargets.min(8) as usize],
            blend = ?g.blend.RenderTarget[0],
            cull = g.rasterizer.CullMode.0,
            "host graphics state"
        );
        Some(HostPipeline {
            compute: false,
            mesh: false,
            pso,
            root,
            stages: bound,
            name: format!("{shaders:?}"),
            slots: g.slots(),
            inputs: g.elements(),
        })
    }
}

/// A mesh pipeline from the title's graphics state: the pipeline-state
/// stream form (no input layout or topology).
fn mesh_pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    ms: D3D12_SHADER_BYTECODE,
    ps: D3D12_SHADER_BYTECODE,
    g: &GraphicsState,
    name: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    use windows::core::Interface;
    /// A stream subobject: its type, then its description, pointer-aligned.
    #[repr(C, align(8))]
    struct Sub<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);
    #[repr(C)]
    struct Stream {
        root: Sub<*mut std::ffi::c_void>,
        ms: Sub<D3D12_SHADER_BYTECODE>,
        ps: Sub<D3D12_SHADER_BYTECODE>,
        blend: Sub<D3D12_BLEND_DESC>,
        mask: Sub<u32>,
        raster: Sub<D3D12_RASTERIZER_DESC>,
        depth: Sub<D3D12_DEPTH_STENCIL_DESC>,
        targets: Sub<D3D12_RT_FORMAT_ARRAY>,
        dsv: Sub<DXGI_FORMAT>,
        samples: Sub<DXGI_SAMPLE_DESC>,
    }
    let mut stream = Stream {
        root: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE, root.as_raw()),
        ms: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, ms),
        ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, ps),
        blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, g.blend),
        mask: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK, g.sample_mask),
        raster: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER, g.rasterizer),
        depth: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL, g.depth),
        targets: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS, g.targets),
        dsv: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT, g.depth_format),
        samples: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC, g.samples),
    };
    let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
        SizeInBytes: size_of::<Stream>(),
        pPipelineStateSubobjectStream: (&raw mut stream).cast(),
    };
    let device = device.cast::<ID3D12Device2>()?;
    super::library::pipeline(
        name,
        // SAFETY: the stream holds whole subobjects pointing at live data.
        |library, name| unsafe { library.LoadPipeline(name, &desc) },
        // SAFETY: as above.
        || unsafe { device.CreatePipelineState(&desc) },
    )
}

/// The bytes of shader code (none for an absent stage).
fn code_bytes(code: &D3D12_SHADER_BYTECODE) -> Vec<u8> {
    if code.pShaderBytecode.is_null() {
        return Vec::new();
    }
    // SAFETY: a translation's own bytes, alive while its shader is.
    unsafe { std::slice::from_raw_parts(code.pShaderBytecode.cast::<u8>(), code.BytecodeLength) }.to_vec()
}

fn root_signature(
    device: &ID3D12Device,
    params: &[D3D12_ROOT_PARAMETER1],
    samplers: &[D3D12_STATIC_SAMPLER_DESC],
    compute: bool,
) -> windows::core::Result<(ID3D12RootSignature, Vec<u8>)> {
    let input = if compute {
        D3D12_ROOT_SIGNATURE_FLAG_NONE
    } else {
        D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
    };
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: windows::Win32::Graphics::Direct3D12::D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: samplers.len() as u32,
                pStaticSamplers: samplers.as_ptr(),
                Flags: input
                    | D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
                    | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
            },
        },
    };
    // SAFETY: the description points at live arrays; the blob is read in
    // full before it is released.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut error: Option<ID3DBlob> = None;
        if let Err(e) = D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut error)) {
            if let Some(error) = error {
                let text = std::slice::from_raw_parts(
                    error.GetBufferPointer().cast::<u8>(),
                    error.GetBufferSize(),
                );
                tracing::warn!("root signature: {}", String::from_utf8_lossy(text));
            }
            return Err(e);
        }
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        Ok((device.CreateRootSignature(0, bytes)?, bytes.to_vec()))
    }
}

#[cfg(all(test, feature = "diagnostics"))]
mod geometry_tests {
    use super::*;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::core::s;

    #[test]
    fn geometry_stage_draws_through_the_guest_state() {
        // Guest triangle list and direct rectangles expanded to host triangles.
        for topology in [4, 17] {
            draw_and_check_target(topology);
        }
    }

    fn draw_and_check_target(topology: u32) {
        let Some(host) = host() else { return };
        let source = r#"
struct V { float4 pos : SV_Position; float4 color : COLOR; };
V vs(uint id : SV_VertexID) {
    V v;
    v.pos = float4(id == 2 ? 3 : -1, id == 1 ? 3 : -1, 0, 1);
    v.color = float4(0, 1, 0, 1);
    return v;
}
[maxvertexcount(3)]
void gs(triangle V input[3], inout TriangleStream<V> stream) {
    for (uint i = 0; i < 3; i++) {
        V v = input[i];
        v.color = v.color.yxzw;
        stream.Append(v);
    }
    stream.RestartStrip();
}
float4 ps(V input) : SV_Target { return input.color; }
"#;
        let shader = |stage, hw, program, entry, profile| {
            let mut code: Option<ID3DBlob> = None;
            // SAFETY: the source lives through the compiler call.
            unsafe {
                D3DCompile(
                    source.as_ptr().cast(),
                    source.len(),
                    s!("geometry_test"),
                    None,
                    None,
                    entry,
                    profile,
                    0,
                    0,
                    &mut code,
                    None,
                )
                .unwrap();
            }
            let code = code.unwrap();
            // SAFETY: the blob owns the shader's bytes.
            let dxil = unsafe {
                std::slice::from_raw_parts(
                    code.GetBufferPointer().cast::<u8>(),
                    code.GetBufferSize(),
                )
            }
            .to_vec();
            let shader = super::super::shader(stage, &dxil);
            let _ = shader.translated[program as usize].set(Some(Translated {
                host: victory_shader::emit::HostShader {
                    stage: hw,
                    dxil,
                    user_data: Vec::new(),
                    samplers: Vec::new(),
                    static_samplers: Vec::new(),
                    root_cbvs: Vec::new(),
                    root_srvs: Vec::new(),
                },
                used: Vec::new(),
            }));
            shader
        };
        use victory_shader::HwStage::{Es, Gs, Ps};
        let shaders = vec![
            shader("vertex", Es, Program::Es, s!("vs"), s!("vs_5_1")),
            shader("geometry", Gs, Program::Gs, s!("gs"), s!("gs_5_1")),
            shader("pixel", Ps, Program::Main, s!("ps"), s!("ps_5_1")),
        ];
        let mut graphics = GraphicsState::default();
        graphics.topology = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
        graphics.depth.DepthEnable = false.into();
        graphics.rasterizer.CullMode = D3D12_CULL_MODE_NONE;
        graphics.targets.NumRenderTargets = 1;
        graphics.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        let null = std::ptr::null();
        let mut pipeline = null;
        assert_eq!(
            super::super::hand_out(
                shaders,
                Some(Box::new(graphics)),
                &super::super::PIPELINE_IIDS[0],
                &mut pipeline
            ),
            0
        );
        // The guest host pipeline builds on a worker.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pso = loop {
            if let Some(pso) = super::super::host_pipeline(pipeline) {
                break pso;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "geometry pipeline did not build"
            );
            std::thread::yield_now();
        };
        assert_eq!(
            pso.stages
                .iter()
                .map(|stage| stage.program)
                .collect::<Vec<_>>(),
            [Program::Es, Program::Gs, Program::Main]
        );
        use windows::Win32::Graphics::Direct3D::{
            D3D_PRIMITIVE_TOPOLOGY_LINELIST, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST_ADJ, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
        };
        assert!(pso.supports_input_topology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST));
        assert!(pso.supports_input_topology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP));
        assert!(!pso.supports_input_topology(D3D_PRIMITIVE_TOPOLOGY_LINELIST));
        assert!(!pso.supports_input_topology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST_ADJ));
        let props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 8,
            Height: 8,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let mut target = None;
        // SAFETY: the target description is complete and the out pointer is ours.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &props,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    None,
                    &mut target,
                )
                .unwrap();
        }
        let target: ID3D12Resource = target.unwrap();
        let list = host.new_list().unwrap();
        let rtv = host.rtv(&target, None);
        let mut state = crate::draw::State::default();
        state.set_pipeline(pipeline);
        state.set_topology(&list, topology);
        crate::draw::viewports(
            &list,
            &[D3D12_VIEWPORT {
                Width: 8.0,
                Height: 8.0,
                MaxDepth: 1.0,
                ..Default::default()
            }],
        );
        crate::draw::scissors(
            &list,
            &[RECT {
                left: 0,
                top: 0,
                right: 8,
                bottom: 8,
            }],
        );
        // SAFETY: a live target in render-target state.
        unsafe {
            list.list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.list.ClearRenderTargetView(rtv, &[0.0; 4], None);
        }
        state.draw(&list, 3, 1, 0, 0);
        // SAFETY: the draw uses the target only as an RTV before the readback.
        unsafe {
            list.list.ResourceBarrier(&[crate::host::transition(
                &target,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
        }
        let (readback, fp) = crate::capture::read_back(&host.device, &list.list, &target).unwrap();
        list.close();
        host.execute(&[&list]);
        let mut data = std::ptr::null_mut();
        // SAFETY: the copy completed and the row pitch covers all the texels.
        unsafe {
            readback.Map(0, None, Some(&mut data)).unwrap();
            let texel = data.cast::<u8>().add(fp.Offset as usize);
            for y in 0..8 {
                let row =
                    std::slice::from_raw_parts(texel.add(y * fp.Footprint.RowPitch as usize), 32);
                assert!(
                    row.chunks_exact(4).all(|rgba| rgba == [255, 0, 0, 255]),
                    "topology {topology}, row {y}"
                );
            }
            readback.Unmap(0, None);
        }
        let release =
            crate::object::slot::<extern "system" fn(crate::object::This) -> u32>(pipeline, 0x10);
        assert_eq!(release(pipeline), 0);
    }
}
