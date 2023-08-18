SELECT
    credential
FROM
    api.webauthn_credential
    JOIN api.account ON account.id = webauthn_credential.account_id
WHERE
    lower(account.username) = lower($1);
