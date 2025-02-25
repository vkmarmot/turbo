use anyhow::Result;
use async_trait::async_trait;
use swc_core::ecma::ast::Program;
use turbo_tasks::primitives::StringVc;
use turbo_tasks_fs::FileSystemPathVc;
use turbopack_core::issue::{Issue, IssueSeverity, IssueSeverityVc, IssueVc};
use turbopack_ecmascript::{CustomTransformer, TransformContext};

/// A wrapper around an SWC's ecma transform wasm plugin module bytes, allowing
/// it to operate with the turbo_tasks caching requirements.
/// Internally this contains a `CompiledPluginModuleBytes`, which points to the
/// compiled, serialized wasmer::Module instead of raw file bytes to reduce the
/// cost of the compilation.
#[turbo_tasks::value(
    transparent,
    serialization = "none",
    eq = "manual",
    into = "new",
    cell = "new"
)]
pub struct SwcPluginModule(
    #[turbo_tasks(trace_ignore)]
    #[cfg(feature = "swc_ecma_transform_plugin")]
    pub swc_core::plugin_runner::plugin_module_bytes::CompiledPluginModuleBytes,
    // Dummy field to avoid turbo_tasks macro complaining about empty struct.
    // This is because we can't import CompiledPluginModuleBytes by default, it should be only
    // available for the target / platforms that support swc plugins (which can build wasmer)
    #[cfg(not(feature = "swc_ecma_transform_plugin"))] pub (),
);

impl SwcPluginModule {
    pub fn new(plugin_name: &str, plugin_bytes: Vec<u8>) -> Self {
        #[cfg(feature = "swc_ecma_transform_plugin")]
        {
            Self({
                use swc_core::plugin_runner::plugin_module_bytes::{
                    CompiledPluginModuleBytes, RawPluginModuleBytes,
                };
                CompiledPluginModuleBytes::from(RawPluginModuleBytes::new(
                    plugin_name.to_string(),
                    plugin_bytes,
                ))
            })
        }

        #[cfg(not(feature = "swc_ecma_transform_plugin"))]
        {
            Self(())
        }
    }
}

#[turbo_tasks::value(shared)]
struct UnsupportedSwcEcmaTransformPluginsIssue {
    pub context: FileSystemPathVc,
}

#[turbo_tasks::value_impl]
impl Issue for UnsupportedSwcEcmaTransformPluginsIssue {
    #[turbo_tasks::function]
    fn severity(&self) -> IssueSeverityVc {
        IssueSeverity::Warning.into()
    }

    #[turbo_tasks::function]
    fn category(&self) -> StringVc {
        StringVc::cell("transform".to_string())
    }

    #[turbo_tasks::function]
    async fn title(&self) -> Result<StringVc> {
        Ok(StringVc::cell(
            "Unsupported SWC EcmaScript transform plugins on this platform.".to_string(),
        ))
    }

    #[turbo_tasks::function]
    fn context(&self) -> FileSystemPathVc {
        self.context
    }

    #[turbo_tasks::function]
    fn description(&self) -> StringVc {
        StringVc::cell(
            "Turbopack does not yet support running SWC EcmaScript transform plugins on this \
             platform."
                .to_string(),
        )
    }
}

/// A custom transformer plugin to execute SWC's transform plugins.
#[derive(Debug)]
pub struct SwcEcmaTransformPluginsTransformer {
    #[cfg(feature = "swc_ecma_transform_plugin")]
    plugins: Vec<(SwcPluginModuleVc, serde_json::Value)>,
}

impl SwcEcmaTransformPluginsTransformer {
    #[cfg(feature = "swc_ecma_transform_plugin")]
    pub fn new(plugins: Vec<(SwcPluginModuleVc, serde_json::Value)>) -> Self {
        Self { plugins }
    }

    // [TODO] Due to WEB-1102 putting this module itself behind compile time feature
    // doesn't work. Instead allow to instantiate dummy instance.
    #[cfg(not(feature = "swc_ecma_transform_plugin"))]
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl CustomTransformer for SwcEcmaTransformPluginsTransformer {
    #[cfg_attr(not(feature = "swc_ecma_transform_plugin"), allow(unused))]
    async fn transform(&self, program: &mut Program, ctx: &TransformContext<'_>) -> Result<()> {
        #[cfg(feature = "swc_ecma_transform_plugin")]
        {
            use std::{cell::RefCell, rc::Rc, sync::Arc};

            use swc_core::{
                common::{
                    comments::SingleThreadedComments,
                    plugin::{
                        metadata::TransformPluginMetadataContext, serialized::PluginSerializedBytes,
                    },
                    util::take::Take,
                },
                ecma::ast::Module,
                plugin::proxies::{HostCommentsStorage, COMMENTS},
                plugin_runner::plugin_module_bytes::PluginModuleBytes,
            };

            let mut plugins = vec![];
            for (plugin_module, config) in &self.plugins {
                let plugin_module = &plugin_module.await?.0;

                plugins.push((
                    plugin_module.get_module_name().to_string(),
                    config.clone(),
                    Box::new(plugin_module.clone()),
                ));
            }

            let should_enable_comments_proxy =
                !ctx.comments.leading.is_empty() && !ctx.comments.trailing.is_empty();

            //[TODO]: as same as swc/core does, we should set should_enable_comments_proxy
            // depends on the src's comments availability. For now, check naively if leading
            // / trailing comments are empty.
            let comments = if should_enable_comments_proxy {
                // Plugin only able to accept singlethreaded comments, interop from
                // multithreaded comments.
                let mut leading =
                    swc_core::common::comments::SingleThreadedCommentsMapInner::default();
                ctx.comments.leading.as_ref().into_iter().for_each(|c| {
                    leading.insert(c.key().clone(), c.value().clone());
                });

                let mut trailing =
                    swc_core::common::comments::SingleThreadedCommentsMapInner::default();
                ctx.comments.trailing.as_ref().into_iter().for_each(|c| {
                    trailing.insert(c.key().clone(), c.value().clone());
                });

                Some(SingleThreadedComments::from_leading_and_trailing(
                    Rc::new(RefCell::new(leading)),
                    Rc::new(RefCell::new(trailing)),
                ))
            } else {
                None
            };

            let transformed_program =
                COMMENTS.set(&HostCommentsStorage { inner: comments }, || {
                    let module_program =
                        std::mem::replace(program, Program::Module(Module::dummy()));
                    let module_program =
                        swc_core::common::plugin::serialized::VersionedSerializable::new(
                            module_program,
                        );
                    let mut serialized_program =
                        PluginSerializedBytes::try_serialize(&module_program)?;

                    let transform_metadata_context = Arc::new(TransformPluginMetadataContext::new(
                        Some(ctx.file_name_str.to_string()),
                        //[TODO]: Support env-related variable injection, i.e process.env.NODE_ENV
                        "development".to_string(),
                        None,
                    ));

                    // Run plugin transformation against current program.
                    // We do not serialize / deserialize between each plugin execution but
                    // copies raw transformed bytes directly into plugin's memory space.
                    // Note: This doesn't mean plugin won't perform any se/deserialization: it
                    // still have to construct from raw bytes internally to perform actual
                    // transform.
                    for (_plugin_name, plugin_config, plugin_module) in plugins.drain(..) {
                        let runtime =
                            swc_core::plugin_runner::wasix_runtime::build_wasi_runtime(None);
                        let mut transform_plugin_executor =
                            swc_core::plugin_runner::create_plugin_transform_executor(
                                &ctx.source_map,
                                &ctx.unresolved_mark,
                                &transform_metadata_context,
                                plugin_module,
                                Some(plugin_config),
                                runtime,
                            );

                        serialized_program = transform_plugin_executor
                            .transform(&serialized_program, Some(should_enable_comments_proxy))?;
                    }

                    serialized_program.deserialize().map(|v| v.into_inner())
                })?;

            *program = transformed_program;
        }

        #[cfg(not(feature = "swc_ecma_transform_plugin"))]
        {
            let issue: UnsupportedSwcEcmaTransformPluginsIssueVc =
                UnsupportedSwcEcmaTransformPluginsIssue {
                    context: ctx.file_path,
                }
                .into();
            issue.as_issue().emit();
        }

        Ok(())
    }
}
