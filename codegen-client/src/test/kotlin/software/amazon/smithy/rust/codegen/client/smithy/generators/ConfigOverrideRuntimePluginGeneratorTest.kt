/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

package software.amazon.smithy.rust.codegen.client.smithy.generators

import org.junit.jupiter.api.Test
import software.amazon.smithy.rust.codegen.client.testutil.TestCodegenSettings
import software.amazon.smithy.rust.codegen.client.testutil.clientIntegrationTest
import software.amazon.smithy.rust.codegen.core.rustlang.CargoDependency
import software.amazon.smithy.rust.codegen.core.rustlang.CargoDependency.Companion.smithyRuntimeApiTestUtil
import software.amazon.smithy.rust.codegen.core.rustlang.rustTemplate
import software.amazon.smithy.rust.codegen.core.smithy.RuntimeType
import software.amazon.smithy.rust.codegen.core.smithy.RuntimeType.Companion.preludeScope
import software.amazon.smithy.rust.codegen.core.testutil.IntegrationTestParams
import software.amazon.smithy.rust.codegen.core.testutil.asSmithyModel
import software.amazon.smithy.rust.codegen.core.testutil.testModule
import software.amazon.smithy.rust.codegen.core.testutil.tokioTest
import software.amazon.smithy.rust.codegen.core.testutil.unitTest

internal class ConfigOverrideRuntimePluginGeneratorTest {
    private val model = """
        namespace com.example
        use aws.protocols#awsJson1_0

        @awsJson1_0
        service HelloService {
            operations: [SayHello],
            version: "1"
        }

        @optionalAuth
        operation SayHello { input: TestInput }
        structure TestInput {
           foo: String,
        }
    """.asSmithyModel()

    @Test
    fun `operation overrides endpoint resolver`() {
        clientIntegrationTest(
            model,
            params = IntegrationTestParams(additionalSettings = TestCodegenSettings.orchestratorMode()),
        ) { clientCodegenContext, rustCrate ->
            val runtimeConfig = clientCodegenContext.runtimeConfig
            val codegenScope = arrayOf(
                *preludeScope,
                "ConfigBagAccessors" to RuntimeType.configBagAccessors(runtimeConfig),
                "EndpointResolverParams" to RuntimeType.smithyRuntimeApi(runtimeConfig)
                    .resolve("client::orchestrator::EndpointResolverParams"),
                "RuntimePlugin" to RuntimeType.runtimePlugin(runtimeConfig),
            )
            rustCrate.testModule {
                addDependency(CargoDependency.Tokio.toDevDependency().withFeature("test-util"))
                tokioTest("test_operation_overrides_endpoint_resolver") {
                    rustTemplate(
                        """
                        use #{RuntimePlugin};
                        use ::aws_smithy_runtime_api::client::orchestrator::EndpointResolver;

                        let expected_url = "http://localhost:1234/";
                        let client_config = crate::config::Config::builder().build();
                        let config_override =
                            crate::config::Config::builder().endpoint_resolver(expected_url);
                        let sut = crate::config::ConfigOverrideRuntimePlugin::new(
                            config_override,
                            client_config.config,
                            &client_config.runtime_components,
                        );
                        let sut_components = sut.runtime_components();
                        let endpoint_resolver = sut_components.endpoint_resolver().unwrap();
                        let endpoint = endpoint_resolver
                            .resolve_endpoint(&#{EndpointResolverParams}::new(crate::config::endpoint::Params {}))
                            .await
                            .unwrap();

                        assert_eq!(expected_url, endpoint.url());
                        """,
                        *codegenScope,
                    )
                }
            }
        }
    }

    @Test
    fun `operation overrides http connector`() {
        clientIntegrationTest(
            model,
            params = IntegrationTestParams(additionalSettings = TestCodegenSettings.orchestratorMode()),
        ) { clientCodegenContext, rustCrate ->
            val runtimeConfig = clientCodegenContext.runtimeConfig
            val codegenScope = arrayOf(
                *preludeScope,
                "ConfigBagAccessors" to RuntimeType.configBagAccessors(runtimeConfig),
                "RuntimePlugin" to RuntimeType.runtimePlugin(runtimeConfig),
            )
            rustCrate.testModule {
                addDependency(CargoDependency.Tokio.toDevDependency().withFeature("test-util"))
                tokioTest("test_operation_overrides_http_connection") {
                    rustTemplate(
                        """
                        use #{AsyncSleep};

                        let (conn, captured_request) = #{capture_request}(#{None});
                        let expected_url = "http://localhost:1234/";
                        let client_config = crate::config::Config::builder()
                            .endpoint_resolver(expected_url)
                            .http_connector(#{NeverConnector}::new())
                            .build();
                        let client = crate::client::Client::from_conf(client_config.clone());
                        let sleep = #{TokioSleep}::new();

                        let send = client.say_hello().send();
                        let timeout = #{Timeout}::new(
                            send,
                            sleep.sleep(::std::time::Duration::from_millis(100)),
                        );

                        // sleep future should win because the other future `send` is backed by `NeverConnector`,
                        // yielding `Err`
                        matches!(timeout.await, Err(_));

                        let client = crate::client::Client::from_conf(client_config);
                        let customizable_send = client
                            .say_hello()
                            .customize()
                            .await
                            .unwrap()
                            .config_override(crate::config::Config::builder().http_connector(conn))
                            .send();

                        let timeout = #{Timeout}::new(
                            customizable_send,
                            sleep.sleep(::std::time::Duration::from_millis(100)),
                        );

                        // `conn` should shadow `NeverConnector` by virtue of `config_override`
                        match timeout.await {
                            Ok(_) => {
                                assert_eq!(
                                    expected_url,
                                    captured_request.expect_request().uri().to_string()
                                );
                            }
                            Err(_) => {
                                panic!("this arm should not be reached since the `customizable_send` future should win");
                            }
                        }
                        """,
                        *codegenScope,
                        "AsyncSleep" to RuntimeType.smithyAsync(runtimeConfig).resolve("rt::sleep::AsyncSleep"),
                        "capture_request" to RuntimeType.captureRequest(runtimeConfig),
                        "NeverConnector" to RuntimeType.smithyClient(runtimeConfig)
                            .resolve("never::NeverConnector"),
                        "Timeout" to RuntimeType.smithyAsync(runtimeConfig).resolve("future::timeout::Timeout"),
                        "TokioSleep" to RuntimeType.smithyAsync(runtimeConfig)
                            .resolve("rt::sleep::TokioSleep"),
                    )
                }
            }
        }
    }

    @Test
    fun `operation overrides retry strategy`() {
        clientIntegrationTest(
            model,
            params = IntegrationTestParams(additionalSettings = TestCodegenSettings.orchestratorMode()),
        ) { clientCodegenContext, rustCrate ->
            val runtimeConfig = clientCodegenContext.runtimeConfig
            val codegenScope = arrayOf(
                *preludeScope,
                "AlwaysRetry" to RuntimeType.smithyRuntimeApi(runtimeConfig)
                    .resolve("client::retries::AlwaysRetry"),
                "ConfigBag" to RuntimeType.smithyTypes(runtimeConfig).resolve("config_bag::ConfigBag"),
                "ConfigBagAccessors" to RuntimeType.configBagAccessors(runtimeConfig),
                "ErrorKind" to RuntimeType.smithyTypes(runtimeConfig).resolve("retry::ErrorKind"),
                "InterceptorContext" to RuntimeType.interceptorContext(runtimeConfig),
                "Layer" to RuntimeType.smithyTypes(runtimeConfig).resolve("config_bag::Layer"),
                "OrchestratorError" to RuntimeType.smithyRuntimeApi(runtimeConfig)
                    .resolve("client::orchestrator::OrchestratorError"),
                "RetryConfig" to RuntimeType.smithyTypes(clientCodegenContext.runtimeConfig)
                    .resolve("retry::RetryConfig"),
                "RequestAttempts" to smithyRuntimeApiTestUtil(runtimeConfig).toType()
                    .resolve("client::request_attempts::RequestAttempts"),
                "RetryClassifiers" to RuntimeType.smithyRuntimeApi(runtimeConfig)
                    .resolve("client::retries::RetryClassifiers"),
                "RuntimeComponentsBuilder" to RuntimeType.runtimeComponentsBuilder(runtimeConfig),
                "RuntimePlugin" to RuntimeType.runtimePlugin(runtimeConfig),
                "ShouldAttempt" to RuntimeType.smithyRuntimeApi(runtimeConfig)
                    .resolve("client::retries::ShouldAttempt"),
                "TypeErasedBox" to RuntimeType.smithyTypes(runtimeConfig).resolve("type_erasure::TypeErasedBox"),
            )
            rustCrate.testModule {
                unitTest("test_operation_overrides_retry_strategy") {
                    rustTemplate(
                        """
                        use #{RuntimePlugin};
                        use ::aws_smithy_runtime_api::client::retries::RetryStrategy;

                        let client_config = crate::config::Config::builder()
                            .retry_config(#{RetryConfig}::standard().with_max_attempts(3))
                            .build();

                        let mut ctx = #{InterceptorContext}::new(#{TypeErasedBox}::new(()));
                        ctx.set_output_or_error(#{Err}(#{OrchestratorError}::other("doesn't matter")));

                        let mut layer = #{Layer}::new("test");
                        layer.store_put(#{RequestAttempts}::new(1));

                        let mut cfg = #{ConfigBag}::of_layers(vec![layer]);
                        let client_config_layer = client_config.config;
                        cfg.push_shared_layer(client_config_layer.clone());

                        let retry_classifiers_component = #{RuntimeComponentsBuilder}::new("retry_classifier")
                            .with_retry_classifiers(#{Some}(
                                #{RetryClassifiers}::new().with_classifier(#{AlwaysRetry}(#{ErrorKind}::TransientError)),
                            ));

                        // Emulate the merging of runtime components from runtime plugins that the orchestrator does
                        let runtime_components = #{RuntimeComponentsBuilder}::for_tests()
                            .merge_from(&client_config.runtime_components)
                            .merge_from(&retry_classifiers_component)
                            .build()
                            .unwrap();

                        let retry = runtime_components.retry_strategy();
                        assert!(matches!(
                            retry.should_attempt_retry(&ctx, &runtime_components, &cfg).unwrap(),
                            #{ShouldAttempt}::YesAfterDelay(_)
                        ));

                        // sets `max_attempts` to 1 implicitly by using `disabled()`, forcing it to run out of
                        // attempts with respect to `RequestAttempts` set to 1 above
                        let config_override_builder = crate::config::Config::builder()
                            .retry_config(#{RetryConfig}::disabled());
                        let config_override = config_override_builder.clone().build();
                        let sut = crate::config::ConfigOverrideRuntimePlugin::new(
                            config_override_builder,
                            client_config_layer,
                            &client_config.runtime_components,
                        );
                        let sut_layer = sut.config().unwrap();
                        cfg.push_shared_layer(sut_layer);

                        // Emulate the merging of runtime components from runtime plugins that the orchestrator does
                        let runtime_components = #{RuntimeComponentsBuilder}::for_tests()
                            .merge_from(&client_config.runtime_components)
                            .merge_from(&retry_classifiers_component)
                            .merge_from(&config_override.runtime_components)
                            .build()
                            .unwrap();

                        let retry = runtime_components.retry_strategy();
                        assert!(matches!(
                            retry.should_attempt_retry(&ctx, &runtime_components, &cfg).unwrap(),
                            #{ShouldAttempt}::No
                        ));
                        """,
                        *codegenScope,
                    )
                }
            }
        }
    }
}
