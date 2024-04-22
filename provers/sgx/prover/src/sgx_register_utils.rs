use alloy_contract::SolCallBuilder;
use alloy_provider::{network::EthereumSigner, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_signer::Signer;
use alloy_sol_types::{sol, SolValue};
use alloy_transport_http::Http;
use pem::parse_many;
use raiko_primitives::{address, hex, Address, Bytes, FixedBytes, U256};
use url::Url;

sol! {
    #[derive(Debug)]
    struct Header {
        bytes2 version;
        bytes2 attestationKeyType;
        bytes4 teeType;
        bytes2 qeSvn;
        bytes2 pceSvn;
        bytes16 qeVendorId;
        bytes20 userData;
    }

    #[derive(Debug)]
    struct EnclaveReport {
        bytes16 cpuSvn;
        bytes4 miscSelect;
        bytes28 reserved1;
        bytes16 attributes;
        bytes32 mrEnclave;
        bytes32 reserved2;
        bytes32 mrSigner;
        bytes reserved3; // 96 bytes
        uint16 isvProdId;
        uint16 isvSvn;
        bytes reserved4; // 60 bytes
        bytes reportData; // 64 bytes - For QEReports, this contains the hash of the concatenation
            // of attestation key and QEAuthData
    }

    #[derive(Debug)]
    struct QEAuthData {
        uint16 parsedDataSize;
        bytes data;
    }

    #[derive(Debug)]
    struct CertificationData {
        uint16 certType;
        // todo! In encoded path, we need to calculate the size of certDataArray
        // certDataSize = len(join((BEGIN_CERT, certArray[i], END_CERT) for i in 0..3))
        // But for plain bytes path, we don't need that.
        uint32 certDataSize;
        bytes[3] decodedCertDataArray; // base64 decoded cert bytes array
    }

    #[derive(Debug)]
    struct ECDSAQuoteV3AuthData {
        bytes ecdsa256BitSignature; // 64 bytes
        bytes ecdsaAttestationKey; // 64 bytes
        EnclaveReport pckSignedQeReport; // 384 bytes
        bytes qeReportSignature; // 64 bytes
        QEAuthData qeAuthData;
        CertificationData certification;
    }

    #[derive(Debug)]
    struct ParsedV3QuoteStruct {
        Header header;
        EnclaveReport localEnclaveReport;
        ECDSAQuoteV3AuthData v3AuthData;
    }

    #[sol(rpc)]
    contract SgxVerifier {
        #[derive(Debug)]
        function registerInstance(ParsedV3QuoteStruct calldata _attestation)
            external
            returns (uint256);
    }
}

fn little_endian_decode(encoded: &[u8]) -> u64 {
    assert!(encoded.len() <= 8, "encoded bytes should be less than 8");
    let mut decoded = 0;
    for (i, byte) in encoded.iter().enumerate() {
        let digits = *byte as u64;
        let upper_digit = digits / 16;
        let lower_digit = digits % 16;

        let acc = lower_digit * (16u64.pow((2 * i) as u32));
        let acc = acc + upper_digit * (16u64.pow(((2 * i) + 1) as u32));

        decoded += acc;
    }

    decoded
}

fn parse_quote_header(quote_bytes: &[u8]) -> Result<Header, Box<dyn std::error::Error>> {
    assert!(quote_bytes.len() > 48, "quote bytes should be at least 48");
    let version = &quote_bytes[0..2];
    let attestation_key_type = &quote_bytes[2..4];
    let tee_type = &quote_bytes[4..8];
    let qe_svn = &quote_bytes[8..10]; // check bytes2(xx)
    let pce_svn = &quote_bytes[10..12];
    let qe_vendor_id = &quote_bytes[12..28];
    let user_data = &quote_bytes[28..48];

    Ok(Header {
        version: FixedBytes::<2>::from_slice(version),
        attestationKeyType: FixedBytes::<2>::from_slice(attestation_key_type),
        teeType: FixedBytes::<4>::from_slice(tee_type),
        qeSvn: FixedBytes::<2>::from_slice(qe_svn),
        pceSvn: FixedBytes::<2>::from_slice(pce_svn),
        qeVendorId: FixedBytes::<16>::from_slice(qe_vendor_id),
        userData: FixedBytes::<20>::from_slice(user_data),
    })
}

fn parse_quote_enclave_report(
    enclave_report_bytes: &[u8],
) -> Result<EnclaveReport, Box<dyn std::error::Error>> {
    let cpu_svn = &enclave_report_bytes[0..16];
    let misc_select = &enclave_report_bytes[16..20];
    let reserved1 = &enclave_report_bytes[20..48];
    let attributes = &enclave_report_bytes[48..64];
    let mr_enclave = &enclave_report_bytes[64..96];
    let reserved2 = &enclave_report_bytes[96..128];
    let mr_signer = &enclave_report_bytes[128..160];
    let reserved3 = &enclave_report_bytes[160..256];
    let isv_prod_id = &enclave_report_bytes[256..258];
    let isv_svn = &enclave_report_bytes[258..260];
    let reserved4 = &enclave_report_bytes[260..320];
    let report_data = &enclave_report_bytes[320..384];

    Ok(EnclaveReport {
        cpuSvn: FixedBytes::<16>::from_slice(cpu_svn),
        miscSelect: FixedBytes::<4>::from_slice(misc_select),
        reserved1: FixedBytes::<28>::from_slice(reserved1),
        attributes: FixedBytes::<16>::from_slice(attributes),
        mrEnclave: FixedBytes::<32>::from_slice(mr_enclave),
        reserved2: FixedBytes::<32>::from_slice(reserved2),
        mrSigner: FixedBytes::<32>::from_slice(mr_signer),
        reserved3: reserved3.to_vec().into(),
        isvProdId: little_endian_decode(isv_prod_id) as u16,
        isvSvn: little_endian_decode(isv_svn) as u16,
        reserved4: reserved4.to_vec().into(),
        reportData: report_data.to_vec().into(),
    })
}

fn parse_cerification_chain_bytes(pem_bytes: &[u8]) -> [Vec<u8>; 3] {
    let pems = parse_many(pem_bytes).unwrap();
    assert_eq!(pems.len(), 3);
    let mut decoded_cert_data_array = [vec![], vec![], vec![]];
    for (i, pem) in pems.iter().enumerate() {
        decoded_cert_data_array[i] = pem.contents().to_vec();
    }
    decoded_cert_data_array
}

fn parse_quote_auth_data(
    quote_bytes: &[u8],
) -> Result<ECDSAQuoteV3AuthData, Box<dyn std::error::Error>> {
    // qeAuthData
    let parsed_data_size = little_endian_decode(&quote_bytes[576..578]);
    let data = &quote_bytes[578..578 + parsed_data_size as usize];

    // cert
    let mut offset = (578 + parsed_data_size) as usize;
    let cert_type = little_endian_decode(&quote_bytes[offset..offset + 2]);
    offset += 2;
    let cert_data_size = little_endian_decode(&quote_bytes[offset..offset + 4]);
    offset += 4;
    let cert_data = &quote_bytes[offset..offset + cert_data_size as usize];
    let decoded_cert_data_array = parse_cerification_chain_bytes(cert_data);

    let ecdsa_sig = &quote_bytes[0..64];
    let ecdsa_attestation_key = &quote_bytes[64..128];
    let raw_qe_report = &quote_bytes[128..512];
    let pck_signed_qe_report = parse_quote_enclave_report(raw_qe_report).unwrap();
    let qe_report_signature = &quote_bytes[512..576];

    Ok(ECDSAQuoteV3AuthData {
        ecdsa256BitSignature: ecdsa_sig.to_vec().into(),
        ecdsaAttestationKey: ecdsa_attestation_key.to_vec().into(),
        pckSignedQeReport: pck_signed_qe_report,
        qeReportSignature: qe_report_signature.to_vec().into(),
        qeAuthData: QEAuthData {
            parsedDataSize: parsed_data_size as u16,
            data: Bytes::from(data.to_vec()),
        },
        certification: CertificationData {
            certType: cert_type as u16,
            certDataSize: cert_data_size as u32,
            decodedCertDataArray: decoded_cert_data_array
                .iter()
                .map(|x| Bytes::from(x.clone()))
                .collect::<Vec<Bytes>>()
                .try_into()
                .unwrap(),
        },
    })
}

fn parse_quote(quote_str: &str) -> ParsedV3QuoteStruct {
    let quote_bytes = hex::decode(quote_str).unwrap();
    let header = parse_quote_header(&quote_bytes).unwrap();
    let local_enclave_report = parse_quote_enclave_report(&quote_bytes[48..432]).unwrap();

    let local_auth_data_size: usize = little_endian_decode(&quote_bytes[432..436]) as usize;
    assert_eq!(
        quote_bytes.len() - 436,
        local_auth_data_size as usize,
        "quote length mismatch"
    );

    let v3_auth_data = parse_quote_auth_data(&quote_bytes[436..]).unwrap();

    ParsedV3QuoteStruct {
        header,
        localEnclaveReport: local_enclave_report,
        v3AuthData: v3_auth_data,
    }
}

pub(crate) async fn register_sgx_instance(
    quote_str: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let wallet: alloy_signer_wallet::LocalWallet =
        "bdba1c1b2745e3097787ee938d2c7f818ff81afd22bda490f2bdd1b599719222"
            .parse()
            .unwrap();
    let parsed_quote = parse_quote(quote_str);
    let provider = ProviderBuilder::new()
        .with_recommended_layers()
        .signer(EthereumSigner::from(wallet))
        .on_builtin("https://l1rpc.hekla.taiko.xyz/")
        .await?;
    let sgx_verifier_addr: Address = address!("532EFBf6D62720D0B2a2Bb9d11066E8588cAE6D9");
    let sgx_verifier_contract = SgxVerifier::new(sgx_verifier_addr, &provider);

    let call_builder = sgx_verifier_contract.registerInstance(parsed_quote);
    // send tx
    let call_return = call_builder.call().await?;
    println!("{call_return:?}"); // doStuffReturn { c: 0x..., d: 0x... }

    Ok(())
}

#[cfg(test)]
mod test {
    use alloy_provider::{
        layers::SignerProvider, PendingTransactionBuilder, Provider, RootProvider,
    };

    use super::*;

    const SAMPLE_QUOTE: &str="03000200000000000a000f00939a7233f79c4ca9940a0db3957f060712ce6af1e4a81e0ecdac427b99bb0295000000000b0b100fffff0000000000000000000000000000000000000000000000000000000000000000000000000000000000000500000000000000e7000000000000003c796d2b94140027ca30ff08946eadc9aec0866247ba1655c6cfd2d5470fe0360000000000000000000000000000000000000000000000000000000000000000763b786f07be6ef823c42b8bd5195590c3df076f492df96a5f83bb33228f048e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001de1f05a31ef99d8bc600a99ce290eafec42b1ec0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ca1000005875e0ce8f620f15ff16e041ed8846d3b7a85e0706b7f393d1f7ee178a4df3b71844b3db54f6147a53fb00b922a008784c2d6c2d04958cde233547596f2cb63fc7277e139f5f2982256989fb65198701d836f8d6f15256ff05d4891bcadae813757a7c09fd1ce02297783baf66b9d97662b5fc38053c34970280bea0eb6e1a7e0b0b100fffff0000000000000000000000000000000000000000000000000000000000000000000000000000000000001500000000000000e70000000000000096b347a64e5a045e27369c26e6dcda51fd7c850e9b3a3a79e718f43261dee1e400000000000000000000000000000000000000000000000000000000000000008c4f5775d796503e96137f77c68a829a0056ac8ded70140b081b094490c57bff00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000035b9ea12f4cf90ec68e8f4b0cbeb15ab6c70e858f1ed8b00c6f3b8471bf1146600000000000000000000000000000000000000000000000000000000000000002a88a769f8865bb1f5aa1a112396618865c9de7da437960ec883dba41d7d6c60a9f81b1e41697fd961f56a1dba150f79cfe30390254ee959c635ebf506020fe22000000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f0500620e00002d2d2d2d2d424547494e2043455254494649434154452d2d2d2d2d0a4d494945387a4343424a6d674177494241674956414c7a2b6a596a7863582b664a6f6d415562434a71676966496f6c364d416f4743437147534d343942414d430a4d484178496a416742674e5642414d4d47556c756447567349464e4857434251513073675547786864475a76636d306751304578476a415942674e5642416f4d0a45556c756447567349454e76636e4276636d4630615739754d5251774567594456515148444174545957353059534244624746795954454c4d416b47413155450a4341774351304578437a414a42674e5642415954416c56544d4234584454497a4d4467794f4445784d544d774e566f5844544d774d4467794f4445784d544d770a4e566f77634445694d434147413155454177775a535735305a5777675530645949464244537942445a584a3061575a70593246305a5445614d426747413155450a43677752535735305a577767513239796347397959585270623234784644415342674e564241634d43314e68626e526849454e7359584a684d517377435159440a5651514944414a445154454c4d416b474131554542684d4356564d775754415442676371686b6a4f5051494242676771686b6a4f50514d4242774e43414151790a734153725336726b656a31344866314a537075504f314e445556797a5842437670316834324631305555304146555767315934386f6542673774764e355832490a54474542357a48426a7a6a76396b755779556a556f344944446a434341776f77487759445652306a42426777466f41556c5739647a62306234656c4153636e550a3944504f4156634c336c5177617759445652306642475177596a42676f46366758495a616148523063484d364c79396863476b7564484a316333526c5a484e6c0a636e5a705932567a4c6d6c75644756734c6d4e766253397a5a3367765932567964476c6d61574e6864476c76626939324e4339775932746a636d772f593245390a6347786864475a76636d306d5a57356a62325270626d63395a4756794d42304741315564446751574242525456365a6c7a31764a6b5953666b4a6a384e69667a0a716761775744414f42674e56485138424166384542414d434273417744415944565230544151482f4241497741444343416a734743537147534962345451454e0a41515343416977776767496f4d42344743697147534962345451454e4151454545503547726745637a6f704e626f4d3073493062744145776767466c42676f710a686b69472b453042445145434d4949425654415142677371686b69472b4530424451454341514942437a415142677371686b69472b45304244514543416749420a437a415142677371686b69472b4530424451454341774942417a415142677371686b69472b4530424451454342414942417a415242677371686b69472b4530420a4451454342514943415038774551594c4b6f5a496876684e41513042416759434167442f4d42414743797147534962345451454e41514948416745414d4241470a43797147534962345451454e41514949416745414d42414743797147534962345451454e4151494a416745414d42414743797147534962345451454e4151494b0a416745414d42414743797147534962345451454e4151494c416745414d42414743797147534962345451454e4151494d416745414d42414743797147534962340a5451454e4151494e416745414d42414743797147534962345451454e4151494f416745414d42414743797147534962345451454e41514950416745414d4241470a43797147534962345451454e41514951416745414d42414743797147534962345451454e415149524167454e4d42384743797147534962345451454e415149530a4242414c43774d442f2f38414141414141414141414141414d42414743697147534962345451454e41514d45416741414d42514743697147534962345451454e0a4151514542674267616741414144415042676f71686b69472b45304244514546436745424d42344743697147534962345451454e415159454545574a7a4f76790a5a45384b336b6a2f48685845612f73775241594b4b6f5a496876684e41513042427a41324d42414743797147534962345451454e415163424151482f4d4241470a43797147534962345451454e415163434151482f4d42414743797147534962345451454e415163444151482f4d416f4743437147534d343942414d43413067410a4d45554349427133767832444e616d5142466d55644d652b6d5059454375383458676f4643674977534a5634634a61544169454134337037747277423830732b0a32697761686d4464416e434d774a56504c69534575774451463856456753773d0a2d2d2d2d2d454e442043455254494649434154452d2d2d2d2d0a2d2d2d2d2d424547494e2043455254494649434154452d2d2d2d2d0a4d4949436c6a4343416a32674177494241674956414a567658633239472b487051456e4a3150517a7a674658433935554d416f4743437147534d343942414d430a4d476778476a415942674e5642414d4d45556c756447567349464e48574342536232393049454e424d526f77474159445651514b4442464a626e526c624342440a62334a7762334a6864476c76626a45554d424947413155454277774c553246756447456751327868636d4578437a414a42674e564241674d416b4e424d5173770a435159445651514745774a56557a4165467730784f4441314d6a45784d4455774d5442614677307a4d7a41314d6a45784d4455774d5442614d484178496a41670a42674e5642414d4d47556c756447567349464e4857434251513073675547786864475a76636d306751304578476a415942674e5642416f4d45556c75644756730a49454e76636e4276636d4630615739754d5251774567594456515148444174545957353059534244624746795954454c4d416b474131554543417743513045780a437a414a42674e5642415954416c56544d466b77457759484b6f5a497a6a3043415159494b6f5a497a6a304441516344516741454e53422f377432316c58534f0a3243757a7078773734654a423732457944476757357258437478327456544c7136684b6b367a2b5569525a436e71523770734f766771466553786c6d546c4a6c0a65546d693257597a33714f42757a43427544416642674e5648534d4547444157674251695a517a575770303069664f44744a5653763141624f536347724442530a42674e5648523845537a424a4d45656752614244686b466f64485277637a6f764c324e6c636e52705a6d6c6a5958526c63793530636e567a6447566b633256790a646d6c6a5a584d75615735305a577775593239744c306c756447567355306459556d397664454e424c6d526c636a416442674e5648513445466751556c5739640a7a62306234656c4153636e553944504f4156634c336c517744675944565230504151482f42415144416745474d42494741315564457745422f7751494d4159420a4166384341514177436759494b6f5a497a6a30454177494452774177524149675873566b6930772b6936565947573355462f32327561586530594a446a3155650a6e412b546a44316169356343494359623153416d4435786b66545670766f34556f79695359787244574c6d5552344349394e4b7966504e2b0a2d2d2d2d2d454e442043455254494649434154452d2d2d2d2d0a2d2d2d2d2d424547494e2043455254494649434154452d2d2d2d2d0a4d4949436a7a4343416a53674177494241674955496d554d316c71644e496e7a6737535655723951477a6b6e42717777436759494b6f5a497a6a3045417749770a614445614d4267474131554541777752535735305a5777675530645949464a766233516751304578476a415942674e5642416f4d45556c756447567349454e760a636e4276636d4630615739754d5251774567594456515148444174545957353059534244624746795954454c4d416b47413155454341774351304578437a414a0a42674e5642415954416c56544d423458445445344d4455794d5445774e4455784d466f58445451354d54497a4d54497a4e546b314f566f77614445614d4267470a4131554541777752535735305a5777675530645949464a766233516751304578476a415942674e5642416f4d45556c756447567349454e76636e4276636d46300a615739754d5251774567594456515148444174545957353059534244624746795954454c4d416b47413155454341774351304578437a414a42674e56424159540a416c56544d466b77457759484b6f5a497a6a3043415159494b6f5a497a6a3044415163445167414543366e45774d4449595a4f6a2f69505773437a61454b69370a314f694f534c52466857476a626e42564a66566e6b59347533496a6b4459594c304d784f346d717379596a6c42616c54565978465032734a424b357a6c4b4f420a757a43427544416642674e5648534d4547444157674251695a517a575770303069664f44744a5653763141624f5363477244425342674e5648523845537a424a0a4d45656752614244686b466f64485277637a6f764c324e6c636e52705a6d6c6a5958526c63793530636e567a6447566b63325679646d6c6a5a584d75615735300a5a577775593239744c306c756447567355306459556d397664454e424c6d526c636a416442674e564851344546675155496d554d316c71644e496e7a673753560a55723951477a6b6e4271777744675944565230504151482f42415144416745474d42494741315564457745422f7751494d4159424166384341514577436759490a4b6f5a497a6a3045417749445351417752674968414f572f35516b522b533943695344634e6f6f774c7550524c735747662f59693747535839344267775477670a41694541344a306c72486f4d732b586f356f2f7358364f39515778485241765a55474f6452513763767152586171493d0a2d2d2d2d2d454e442043455254494649434154452d2d2d2d2d0a00";

    #[test]
    fn test_parse_quote() {
        let parsed_quote = parse_quote(SAMPLE_QUOTE);
        println!("{:?}", parsed_quote);
    }

    #[test]
    fn test_tx_call_register() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(test_register_sgx_instance(SAMPLE_QUOTE))
            .unwrap();
    }

    async fn test_register_sgx_instance(quote_str: &str) -> Result<(), Box<dyn std::error::Error>> {
        let rpc_url = "https://l1rpc.hekla.taiko.xyz/";
        let http = Http::new(Url::parse(&rpc_url).expect("invalid rpc url"));
        let mut wallet: alloy_signer_wallet::LocalWallet =
            "18ff81afd22bda490f2bd1b2745e309bdba1c38d2c7f87787ee9d1b599719222"
                .parse()
                .unwrap();
        wallet.set_chain_id(Some(17000));
        println!("wallet: {:?}", wallet);
        let parsed_quote = parse_quote(quote_str);
        let provider = ProviderBuilder::new()
            .signer(EthereumSigner::from(wallet.clone()))
            .with_recommended_layers()
            .provider(RootProvider::new(RpcClient::new(http, false)));
        let sgx_verifier_addr: Address = address!("532EFBf6D62720D0B2a2Bb9d11066E8588cAE6D9");
        let sgx_verifier_contract = SgxVerifier::new(sgx_verifier_addr, &provider);

        let balance = provider.get_balance(wallet.address(), None).await?;
        let nonce = provider
            .get_transaction_count(wallet.address(), None)
            .await?;
        let gas_price = provider.get_gas_price().await?;
        let gas_limit = U256::from(4000000u64);

        let call_builder = sgx_verifier_contract
            .registerInstance(parsed_quote)
            .from(wallet.address())
            .nonce(nonce.as_limbs()[0])
            .value(U256::from(0))
            .gas_price(gas_price)
            .gas(gas_limit);

        assert!(
            balance > gas_price * gas_limit,
            "insufficient balance to send tx"
        );
        // query tx for any error
        let query_call_return = call_builder.call().await?;
        println!("query call return: {query_call_return:?}");

        // send tx & wait the result
        let tx_hash = call_builder
            .send()
            .await?
            .with_required_confirmations(2)
            .with_timeout(Some(std::time::Duration::from_secs(90)))
            .watch()
            .await?;
        println!("call return: {tx_hash:?}");

        let tx_receipt = provider
            .get_transaction_receipt(tx_hash)
            .await
            .expect("tx_hash is valid");
        println!("tx receipt: {tx_receipt:?}");
        let log = tx_receipt
            .map(|x| x.inner.as_receipt().unwrap().logs.first().unwrap().clone())
            .unwrap();
        println!("register sgx instance id: {:?}", log.topics()[1]);

        Ok(())
    }
}
