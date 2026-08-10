package dev.daimonos.remote.crypto

import java.util.Base64
import org.junit.Assert.assertEquals
import org.junit.Test

class Ed25519DeviceIdentityTest {
    @Test
    fun fixedSeedMatchesBackendKnownAnswerVector() {
        val identity = Ed25519DeviceIdentity(ByteArray(32) { 42 })
        val message = authMessage("fixture-challenge", "fixture-ticket")

        assertEquals(
            "GX9rI-FshTLGq8g4-s1ep4m-DHaykgM0A5v6iz02jWE",
            identity.encodedPublicKey,
        )
        assertEquals(
            "y9SPDpaLah9vyhD1FRJ9cp0hL3deHu-_-AYHNLa99bWGeP1BH0qyFd14vFkY4VMnkOkfLsR1oIippOr3J7o9Dg",
            Base64.getUrlEncoder().withoutPadding().encodeToString(identity.sign(message)),
        )
    }
}
