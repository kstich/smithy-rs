/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

package software.amazon.smithy.rust.codegen.client.smithy.generators.protocol

import software.amazon.smithy.codegen.core.CodegenException
import software.amazon.smithy.model.knowledge.OperationIndex
import software.amazon.smithy.model.shapes.DoubleShape
import software.amazon.smithy.model.shapes.FloatShape
import software.amazon.smithy.model.shapes.OperationShape
import software.amazon.smithy.model.shapes.StructureShape
import software.amazon.smithy.model.traits.ErrorTrait
import software.amazon.smithy.protocoltests.traits.AppliesTo
import software.amazon.smithy.protocoltests.traits.HttpMessageTestCase
import software.amazon.smithy.protocoltests.traits.HttpRequestTestCase
import software.amazon.smithy.protocoltests.traits.HttpRequestTestsTrait
import software.amazon.smithy.protocoltests.traits.HttpResponseTestCase
import software.amazon.smithy.protocoltests.traits.HttpResponseTestsTrait
import software.amazon.smithy.rust.codegen.client.smithy.ClientCodegenContext
import software.amazon.smithy.rust.codegen.client.smithy.ClientRustModule
import software.amazon.smithy.rust.codegen.client.smithy.customizations.EndpointPrefixGenerator
import software.amazon.smithy.rust.codegen.client.smithy.generators.ClientInstantiator
import software.amazon.smithy.rust.codegen.core.rustlang.Attribute
import software.amazon.smithy.rust.codegen.core.rustlang.Attribute.Companion.allow
import software.amazon.smithy.rust.codegen.core.rustlang.CargoDependency
import software.amazon.smithy.rust.codegen.core.rustlang.RustModule
import software.amazon.smithy.rust.codegen.core.rustlang.RustWriter
import software.amazon.smithy.rust.codegen.core.rustlang.Writable
import software.amazon.smithy.rust.codegen.core.rustlang.escape
import software.amazon.smithy.rust.codegen.core.rustlang.rust
import software.amazon.smithy.rust.codegen.core.rustlang.rustBlock
import software.amazon.smithy.rust.codegen.core.rustlang.rustTemplate
import software.amazon.smithy.rust.codegen.core.rustlang.withBlock
import software.amazon.smithy.rust.codegen.core.rustlang.writable
import software.amazon.smithy.rust.codegen.core.smithy.RuntimeType
import software.amazon.smithy.rust.codegen.core.smithy.generators.protocol.ProtocolSupport
import software.amazon.smithy.rust.codegen.core.util.dq
import software.amazon.smithy.rust.codegen.core.util.getTrait
import software.amazon.smithy.rust.codegen.core.util.hasTrait
import software.amazon.smithy.rust.codegen.core.util.inputShape
import software.amazon.smithy.rust.codegen.core.util.isStreaming
import software.amazon.smithy.rust.codegen.core.util.orNull
import software.amazon.smithy.rust.codegen.core.util.outputShape
import software.amazon.smithy.rust.codegen.core.util.toSnakeCase
import java.util.logging.Logger

data class ClientCreationParams(
    val codegenContext: ClientCodegenContext,
    val connectorName: String,
    val configBuilderName: String,
    val clientName: String,
)

interface ProtocolTestGenerator {
    val codegenContext: ClientCodegenContext
    val protocolSupport: ProtocolSupport
    val operationShape: OperationShape

    fun render(writer: RustWriter)
}

/**
 * Generate protocol tests for an operation
 */
class DefaultProtocolTestGenerator(
    override val codegenContext: ClientCodegenContext,
    override val protocolSupport: ProtocolSupport,
    override val operationShape: OperationShape,

    private val renderClientCreation: RustWriter.(ClientCreationParams) -> Unit = { params ->
        if (params.codegenContext.smithyRuntimeMode.defaultToMiddleware) {
            rustTemplate(
                """
                let smithy_client = #{Builder}::new()
                    .connector(${params.connectorName})
                    .middleware(#{MapRequestLayer}::for_mapper(#{SmithyEndpointStage}::new()))
                    .build();
                let ${params.clientName} = #{Client}::with_config(smithy_client, ${params.configBuilderName}.build());
                """,
                "Client" to ClientRustModule.root.toType().resolve("Client"),
                "Builder" to ClientRustModule.client.toType().resolve("Builder"),
                "SmithyEndpointStage" to RuntimeType.smithyHttp(codegenContext.runtimeConfig)
                    .resolve("endpoint::middleware::SmithyEndpointStage"),
                "MapRequestLayer" to RuntimeType.smithyHttpTower(codegenContext.runtimeConfig)
                    .resolve("map_request::MapRequestLayer"),
            )
        } else {
            rustTemplate(
                """
                let ${params.clientName} = #{Client}::from_conf(
                    ${params.configBuilderName}
                        .http_connector(${params.connectorName})
                        .build()
                );
                """,
                "Client" to ClientRustModule.root.toType().resolve("Client"),
            )
        }
    },
) : ProtocolTestGenerator {
    private val logger = Logger.getLogger(javaClass.name)

    private val inputShape = operationShape.inputShape(codegenContext.model)
    private val outputShape = operationShape.outputShape(codegenContext.model)
    private val operationSymbol = codegenContext.symbolProvider.toSymbol(operationShape)
    private val operationIndex = OperationIndex.of(codegenContext.model)

    private val instantiator = ClientInstantiator(codegenContext)

    private val codegenScope = arrayOf(
        "SmithyHttp" to RuntimeType.smithyHttp(codegenContext.runtimeConfig),
        "AssertEq" to RuntimeType.PrettyAssertions.resolve("assert_eq!"),
    )

    sealed class TestCase {
        abstract val testCase: HttpMessageTestCase

        data class RequestTest(override val testCase: HttpRequestTestCase) : TestCase()
        data class ResponseTest(override val testCase: HttpResponseTestCase, val targetShape: StructureShape) :
            TestCase()
    }

    override fun render(writer: RustWriter) {
        val requestTests = operationShape.getTrait<HttpRequestTestsTrait>()
            ?.getTestCasesFor(AppliesTo.CLIENT).orEmpty().map { TestCase.RequestTest(it) }
        val responseTests = operationShape.getTrait<HttpResponseTestsTrait>()
            ?.getTestCasesFor(AppliesTo.CLIENT).orEmpty().map { TestCase.ResponseTest(it, outputShape) }
        val errorTests = operationIndex.getErrors(operationShape).flatMap { error ->
            val testCases = error.getTrait<HttpResponseTestsTrait>()
                ?.getTestCasesFor(AppliesTo.CLIENT).orEmpty()
            testCases.map { TestCase.ResponseTest(it, error) }
        }
        val allTests: List<TestCase> = (requestTests + responseTests + errorTests).filterMatching()
        if (allTests.isNotEmpty()) {
            val operationName = operationSymbol.name
            val testModuleName = "${operationName.toSnakeCase()}_request_test"
            val additionalAttributes = listOf(
                Attribute(allow("unreachable_code", "unused_variables")),
            )
            writer.withInlineModule(
                RustModule.inlineTests(testModuleName, additionalAttributes = additionalAttributes),
                null,
            ) {
                renderAllTestCases(allTests)
            }
        }
    }

    private fun RustWriter.renderAllTestCases(allTests: List<TestCase>) {
        allTests.forEach {
            renderTestCaseBlock(it.testCase, this) {
                when (it) {
                    is TestCase.RequestTest -> this.renderHttpRequestTestCase(it.testCase)
                    is TestCase.ResponseTest -> this.renderHttpResponseTestCase(it.testCase, it.targetShape)
                }
            }
        }
    }

    /**
     * Filter out test cases that are disabled or don't match the service protocol
     */
    private fun List<TestCase>.filterMatching(): List<TestCase> {
        return if (RunOnly.isNullOrEmpty()) {
            this.filter { testCase ->
                testCase.testCase.protocol == codegenContext.protocol &&
                    !DisableTests.contains(testCase.testCase.id)
            }
        } else {
            this.filter { RunOnly.contains(it.testCase.id) }
        }
    }

    private fun renderTestCaseBlock(
        testCase: HttpMessageTestCase,
        testModuleWriter: RustWriter,
        block: Writable,
    ) {
        testModuleWriter.newlinePrefix = "/// "
        testCase.documentation.map {
            testModuleWriter.writeWithNoFormatting(it)
        }
        testModuleWriter.write("Test ID: ${testCase.id}")
        testModuleWriter.newlinePrefix = ""
        Attribute.TokioTest.render(testModuleWriter)
        val action = when (testCase) {
            is HttpResponseTestCase -> Action.Response
            is HttpRequestTestCase -> Action.Request
            else -> throw CodegenException("unknown test case type")
        }
        if (expectFail(testCase)) {
            testModuleWriter.writeWithNoFormatting("#[should_panic]")
        }
        val fnName = when (action) {
            is Action.Response -> "_response"
            is Action.Request -> "_request"
        }
        Attribute.AllowUnusedMut.render(testModuleWriter)
        testModuleWriter.rustBlock("async fn ${testCase.id.toSnakeCase()}$fnName()") {
            block(this)
        }
    }

    private fun RustWriter.renderHttpRequestTestCase(
        httpRequestTestCase: HttpRequestTestCase,
    ) {
        if (!protocolSupport.requestSerialization) {
            rust("/* test case disabled for this protocol (not yet supported) */")
            return
        }
        val customParams = httpRequestTestCase.vendorParams.getObjectMember("endpointParams").orNull()?.let { params ->
            writable {
                val customizations = codegenContext.rootDecorator.endpointCustomizations(codegenContext)
                params.getObjectMember("builtInParams").orNull()?.members?.forEach { (name, value) ->
                    customizations.firstNotNullOf {
                        it.setBuiltInOnServiceConfig(name.value, value, "config_builder")
                    }(this)
                }
            }
        } ?: writable { }
        rustTemplate(
            """
            let (conn, request_receiver) = #{capture_request}(None);
            let config_builder = #{config}::Config::builder().with_test_defaults().endpoint_resolver("https://example.com");
            #{customParams}

            """,
            "capture_request" to CargoDependency.smithyClient(codegenContext.runtimeConfig)
                .toDevDependency()
                .withFeature("test-util")
                .toType()
                .resolve("test_connection::capture_request"),
            "config" to ClientRustModule.config,
            "customParams" to customParams,
        )
        renderClientCreation(this, ClientCreationParams(codegenContext, "conn", "config_builder", "client"))

        writeInline("let result = ")
        instantiator.renderFluentCall(this, "client", operationShape, inputShape, httpRequestTestCase.params)
        rust(""".send().await;""")
        // Response parsing will always fail since we feed it an empty response body, so we don't care
        // if it fails, but it is helpful to print what that failure was for debugging
        rust("let _ = dbg!(result);")
        rust("""let http_request = request_receiver.expect_request();""")

        with(httpRequestTestCase) {
            // Override the endpoint for tests that set a `host`, for example:
            // https://github.com/awslabs/smithy/blob/be68f3bbdfe5bf50a104b387094d40c8069f16b1/smithy-aws-protocol-tests/model/restJson1/endpoint-paths.smithy#L19
            host.orNull()?.also { host ->
                val withScheme = "http://$host"
                when (val bindings = EndpointPrefixGenerator.endpointTraitBindings(codegenContext, operationShape)) {
                    null -> rust("let endpoint_prefix = None;")
                    else -> {
                        withBlock("let input = ", ";") {
                            instantiator.render(this@renderHttpRequestTestCase, inputShape, httpRequestTestCase.params)
                        }
                        withBlock("let endpoint_prefix = Some({", "}.unwrap());") {
                            bindings.render(this, "input", codegenContext.smithyRuntimeMode, generateValidation = false)
                        }
                    }
                }
                rustTemplate(
                    """
                    let mut http_request = http_request;
                    let ep = #{SmithyHttp}::endpoint::Endpoint::mutable(${withScheme.dq()}).expect("valid endpoint");
                    ep.set_endpoint(http_request.uri_mut(), endpoint_prefix.as_ref()).expect("valid endpoint");
                    """,
                    *codegenScope,
                )
            }
            rustTemplate(
                """
                #{AssertEq}(http_request.method(), ${method.dq()});
                #{AssertEq}(http_request.uri().path(), ${uri.dq()});
                """,
                *codegenScope,
            )
            resolvedHost.orNull()?.also { host ->
                rustTemplate(
                    """#{AssertEq}(http_request.uri().host().expect("host should be set"), ${host.dq()});""",
                    *codegenScope,
                )
            }
        }
        checkQueryParams(this, httpRequestTestCase.queryParams)
        checkForbidQueryParams(this, httpRequestTestCase.forbidQueryParams)
        checkRequiredQueryParams(this, httpRequestTestCase.requireQueryParams)
        checkHeaders(this, "http_request.headers()", httpRequestTestCase.headers)
        checkForbidHeaders(this, "http_request.headers()", httpRequestTestCase.forbidHeaders)
        checkRequiredHeaders(this, "http_request.headers()", httpRequestTestCase.requireHeaders)
        if (protocolSupport.requestBodySerialization) {
            // "If no request body is defined, then no assertions are made about the body of the message."
            httpRequestTestCase.body.orNull()?.also { body ->
                checkBody(this, body, httpRequestTestCase.bodyMediaType.orNull())
            }
        }

        // Explicitly warn if the test case defined parameters that we aren't doing anything with
        with(httpRequestTestCase) {
            if (authScheme.isPresent) {
                logger.warning("Test case provided authScheme but this was ignored")
            }
            if (!httpRequestTestCase.vendorParams.isEmpty) {
                logger.warning("Test case provided vendorParams but these were ignored")
            }
        }
    }

    private fun HttpMessageTestCase.action(): Action = when (this) {
        is HttpRequestTestCase -> Action.Request
        is HttpResponseTestCase -> Action.Response
        else -> throw CodegenException("Unknown test case type")
    }

    private fun expectFail(testCase: HttpMessageTestCase): Boolean = ExpectFail.find {
        it.id == testCase.id && it.action == testCase.action() && it.service == codegenContext.serviceShape.id.toString()
    } != null

    private fun RustWriter.renderHttpResponseTestCase(
        testCase: HttpResponseTestCase,
        expectedShape: StructureShape,
    ) {
        if (!protocolSupport.responseDeserialization || (
                !protocolSupport.errorDeserialization && expectedShape.hasTrait(
                    ErrorTrait::class.java,
                )
                )
        ) {
            rust("/* test case disabled for this protocol (not yet supported) */")
            return
        }
        writeInline("let expected_output =")
        instantiator.render(this, expectedShape, testCase.params)
        write(";")
        write("let mut http_response = #T::new()", RuntimeType.HttpResponseBuilder)
        testCase.headers.forEach { (key, value) ->
            writeWithNoFormatting(".header(${key.dq()}, ${value.dq()})")
        }
        rust(
            """
            .status(${testCase.code})
            .body(#T::from(${testCase.body.orNull()?.dq()?.replace("#", "##") ?: "vec![]"}))
            .unwrap();
            """,
            RuntimeType.sdkBody(runtimeConfig = codegenContext.runtimeConfig),
        )
        if (codegenContext.smithyRuntimeMode.defaultToMiddleware) {
            rust(
                "let mut op_response = #T::new(http_response);",
                RuntimeType.operationModule(codegenContext.runtimeConfig).resolve("Response"),
            )
            rustTemplate(
                """
                use #{parse_http_response};
                let parser = #{op}::new();
                let parsed = parser.parse_unloaded(&mut op_response);
                let parsed = parsed.unwrap_or_else(|| {
                    let (http_response, _) = op_response.into_parts();
                    let http_response = http_response.map(|body|#{copy_from_slice}(body.bytes().unwrap()));
                    <#{op} as #{parse_http_response}>::parse_loaded(&parser, &http_response)
                });
                """,
                "op" to operationSymbol,
                "copy_from_slice" to RuntimeType.Bytes.resolve("copy_from_slice"),
                "parse_http_response" to RuntimeType.parseHttpResponse(codegenContext.runtimeConfig),
            )
        } else {
            rustTemplate(
                """
                use #{ResponseDeserializer};
                let de = #{OperationDeserializer};
                let parsed = de.deserialize_streaming(&mut http_response);
                let parsed = parsed.unwrap_or_else(|| {
                    let http_response = http_response.map(|body| {
                        #{SdkBody}::from(#{copy_from_slice}(body.bytes().unwrap()))
                    });
                    de.deserialize_nonstreaming(&http_response)
                });
                """,
                "OperationDeserializer" to codegenContext.symbolProvider.moduleForShape(operationShape).toType()
                    .resolve("${operationSymbol.name}ResponseDeserializer"),
                "copy_from_slice" to RuntimeType.Bytes.resolve("copy_from_slice"),
                "ResponseDeserializer" to CargoDependency.smithyRuntimeApi(codegenContext.runtimeConfig).toType()
                    .resolve("client::orchestrator::ResponseDeserializer"),
                "SdkBody" to RuntimeType.sdkBody(codegenContext.runtimeConfig),
            )
        }
        if (expectedShape.hasTrait<ErrorTrait>()) {
            val errorSymbol = codegenContext.symbolProvider.symbolForOperationError(operationShape)
            val errorVariant = codegenContext.symbolProvider.toSymbol(expectedShape).name
            rust("""let parsed = parsed.expect_err("should be error response");""")
            if (codegenContext.smithyRuntimeMode.defaultToOrchestrator) {
                rustTemplate(
                    """let parsed: &#{Error} = parsed.as_operation_error().expect("operation error").downcast_ref().unwrap();""",
                    "Error" to codegenContext.symbolProvider.symbolForOperationError(operationShape),
                )
            }
            rustBlock("if let #T::$errorVariant(parsed) = parsed", errorSymbol) {
                compareMembers(expectedShape)
            }
            rustBlock("else") {
                rust("panic!(\"wrong variant: Got: {:?}. Expected: {:?}\", parsed, expected_output);")
            }
        } else {
            if (codegenContext.smithyRuntimeMode.defaultToMiddleware) {
                rust("let parsed = parsed.unwrap();")
            } else {
                rustTemplate(
                    """let parsed: #{Output} = *parsed.expect("should be successful response").downcast().unwrap();""",
                    "Output" to codegenContext.symbolProvider.toSymbol(expectedShape),
                )
            }
            compareMembers(outputShape)
        }
    }

    private fun RustWriter.compareMembers(shape: StructureShape) {
        shape.members().forEach { member ->
            val memberName = codegenContext.symbolProvider.toMemberName(member)
            if (member.isStreaming(codegenContext.model)) {
                rustTemplate(
                    """
                    #{AssertEq}(
                        parsed.$memberName.collect().await.unwrap().into_bytes(),
                        expected_output.$memberName.collect().await.unwrap().into_bytes()
                    );
                    """,
                    *codegenScope,
                )
            } else {
                when (codegenContext.model.expectShape(member.target)) {
                    is DoubleShape, is FloatShape -> {
                        addUseImports(
                            RuntimeType.protocolTest(codegenContext.runtimeConfig, "FloatEquals").toSymbol(),
                        )
                        rust(
                            """
                            assert!(parsed.$memberName.float_equals(&expected_output.$memberName),
                                "Unexpected value for `$memberName` {:?} vs. {:?}", expected_output.$memberName, parsed.$memberName);
                            """,
                        )
                    }

                    else ->
                        rustTemplate(
                            """#{AssertEq}(parsed.$memberName, expected_output.$memberName, "Unexpected value for `$memberName`");""",
                            *codegenScope,
                        )
                }
            }
        }
    }

    private fun checkBody(rustWriter: RustWriter, body: String, mediaType: String?) {
        rustWriter.write("""let body = http_request.body().bytes().expect("body should be strict");""")
        if (body == "") {
            rustWriter.rustTemplate(
                """
                // No body
                #{AssertEq}(::std::str::from_utf8(body).unwrap(), "");
                """,
                *codegenScope,
            )
        } else {
            // When we generate a body instead of a stub, drop the trailing `;` and enable the assertion
            assertOk(rustWriter) {
                rustWriter.write(
                    "#T(&body, ${
                        rustWriter.escape(body).dq()
                    }, #T::from(${(mediaType ?: "unknown").dq()}))",
                    RuntimeType.protocolTest(codegenContext.runtimeConfig, "validate_body"),
                    RuntimeType.protocolTest(codegenContext.runtimeConfig, "MediaType"),
                )
            }
        }
    }

    private fun checkRequiredHeaders(rustWriter: RustWriter, actualExpression: String, requireHeaders: List<String>) {
        basicCheck(
            requireHeaders,
            rustWriter,
            "required_headers",
            actualExpression,
            "require_headers",
        )
    }

    private fun checkForbidHeaders(rustWriter: RustWriter, actualExpression: String, forbidHeaders: List<String>) {
        basicCheck(
            forbidHeaders,
            rustWriter,
            "forbidden_headers",
            actualExpression,
            "forbid_headers",
        )
    }

    private fun checkHeaders(rustWriter: RustWriter, actualExpression: String, headers: Map<String, String>) {
        if (headers.isEmpty()) {
            return
        }
        val variableName = "expected_headers"
        rustWriter.withBlock("let $variableName = [", "];") {
            writeWithNoFormatting(
                headers.entries.joinToString(",") {
                    "(${it.key.dq()}, ${it.value.dq()})"
                },
            )
        }
        assertOk(rustWriter) {
            write(
                "#T($actualExpression, $variableName)",
                RuntimeType.protocolTest(codegenContext.runtimeConfig, "validate_headers"),
            )
        }
    }

    private fun checkRequiredQueryParams(
        rustWriter: RustWriter,
        requiredParams: List<String>,
    ) = basicCheck(
        requiredParams,
        rustWriter,
        "required_params",
        "&http_request",
        "require_query_params",
    )

    private fun checkForbidQueryParams(
        rustWriter: RustWriter,
        forbidParams: List<String>,
    ) = basicCheck(
        forbidParams,
        rustWriter,
        "forbid_params",
        "&http_request",
        "forbid_query_params",
    )

    private fun checkQueryParams(
        rustWriter: RustWriter,
        queryParams: List<String>,
    ) = basicCheck(
        queryParams,
        rustWriter,
        "expected_query_params",
        "&http_request",
        "validate_query_string",
    )

    private fun basicCheck(
        params: List<String>,
        rustWriter: RustWriter,
        expectedVariableName: String,
        actualExpression: String,
        checkFunction: String,
    ) {
        if (params.isEmpty()) {
            return
        }
        rustWriter.withBlock("let $expectedVariableName = ", ";") {
            strSlice(this, params)
        }
        assertOk(rustWriter) {
            write(
                "#T($actualExpression, $expectedVariableName)",
                RuntimeType.protocolTest(codegenContext.runtimeConfig, checkFunction),
            )
        }
    }

    /**
     * wraps `inner` in a call to `aws_smithy_protocol_test::assert_ok`, a convenience wrapper
     * for pretty printing protocol test helper results
     */
    private fun assertOk(rustWriter: RustWriter, inner: Writable) {
        rustWriter.write("#T(", RuntimeType.protocolTest(codegenContext.runtimeConfig, "assert_ok"))
        inner(rustWriter)
        rustWriter.write(");")
    }

    private fun strSlice(writer: RustWriter, args: List<String>) {
        writer.withBlock("&[", "]") {
            write(args.joinToString(",") { it.dq() })
        }
    }

    companion object {
        sealed class Action {
            object Request : Action()
            object Response : Action()
        }

        data class FailingTest(val service: String, val id: String, val action: Action)

        // These tests fail due to shortcomings in our implementation.
        // These could be configured via runtime configuration, but since this won't be long-lasting,
        // it makes sense to do the simplest thing for now.
        // The test will _fail_ if these pass, so we will discover & remove if we fix them by accident
        private val JsonRpc10 = "aws.protocoltests.json10#JsonRpc10"
        private val AwsJson11 = "aws.protocoltests.json#JsonProtocol"
        private val RestJson = "aws.protocoltests.restjson#RestJson"
        private val RestXml = "aws.protocoltests.restxml#RestXml"
        private val AwsQuery = "aws.protocoltests.query#AwsQuery"
        private val Ec2Query = "aws.protocoltests.ec2#AwsEc2"
        private val ExpectFail = setOf<FailingTest>()
        private val RunOnly: Set<String>? = null

        // These tests are not even attempted to be generated, either because they will not compile
        // or because they are flaky
        private val DisableTests = setOf<String>()
    }
}
