/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

import org.junit.jupiter.api.Test
import software.amazon.smithy.rust.codegen.core.testutil.asSmithyModel
import software.amazon.smithy.rustsdk.awsSdkIntegrationTest

class SdkCodegenIntegrationTest {
    val model = """
        namespace test

        use aws.api#service
        use aws.auth#sigv4
        use aws.protocols#restJson1
        use smithy.rules#endpointRuleSet

        @service(sdkId: "dontcare")
        @restJson1
        @sigv4(name: "dontcare")
        @auth([sigv4])
        @endpointRuleSet({
            "version": "1.0",
            "rules": [{ "type": "endpoint", "conditions": [], "endpoint": { "url": "https://example.com" } }],
            "parameters": {
                "Region": { "required": false, "type": "String", "builtIn": "AWS::Region" },
            }
        })
        service TestService {
            version: "2023-01-01",
            operations: [SomeOperation]
        }

        structure SomeOutput {
            someAttribute: Long,
            someVal: String
        }

        @http(uri: "/SomeOperation", method: "GET")
        @optionalAuth
        operation SomeOperation {
            output: SomeOutput
        }
    """.asSmithyModel()

    @Test
    fun smokeTestSdkCodegen() {
        awsSdkIntegrationTest(
            model,
            defaultToOrchestrator = true,
        ) { _, _ -> /* it should compile */ }
    }

    @Test
    fun smokeTestSdkCodegenMiddleware() {
        awsSdkIntegrationTest(
            model,
            defaultToOrchestrator = false,
        ) { _, _ -> /* it should compile */ }
    }
}
