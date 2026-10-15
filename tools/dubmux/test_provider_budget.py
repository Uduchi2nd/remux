"""Offline tests for admission limits. No provider links are resolved."""
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import tempfile
import unittest
import uuid

from provider_budget import Ledger, Limited, MediaReads, Policy


class ProviderBudgetTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.path = self.root / 'budget.sqlite'
        self.policies = {'premiumize':Policy(220,1,20,5), 'torbox':Policy(2200,3,0,0)}
        self.ledger = Ledger(self.path, self.policies)

    def test_budgets_persist_and_failed_reads_do_not_refund(self):
        token = self.ledger.reserve('premiumize',100,now=1000)
        self.ledger.release(str(uuid.UUID(token)))
        with self.assertRaisesRegex(Limited,'start_rate'):
            self.ledger.reserve('premiumize',100,now=1001)
        token = self.ledger.reserve('premiumize',100,now=1021)
        self.ledger.release(token)
        reopened = Ledger(self.path, self.policies)
        self.assertEqual(reopened.status(1050)['providers']['premiumize']['reserved_bytes'],220)
        with self.assertRaisesRegex(Limited,'daily_reservation_budget'):
            reopened.reserve('premiumize',1,now=1100)
        reopened.reserve('premiumize',100,now=86400+1000)
        self.assertEqual(reopened.status(86400+1000)['providers']['premiumize']['reserved_bytes'],110)

    def test_check_only_never_spends_or_changes_start_cadence(self):
        self.ledger.reserve('premiumize',100,now=1000,check_only=True)
        status=self.ledger.status(1000)['providers']['premiumize']
        self.assertEqual(status['active'],0)
        self.assertEqual(status['reserved_bytes'],0)
        self.ledger.reserve('premiumize',100,now=1000)
        with self.assertRaisesRegex(Limited,'concurrency'):
            self.ledger.reserve('premiumize',100,now=1001,check_only=True)
        self.assertEqual(self.ledger.status(1001)['providers']['premiumize']['reserved_bytes'],110)

    def test_concurrency_denial_does_not_spend_and_providers_are_independent(self):
        token = self.ledger.reserve('premiumize',100,now=1000)
        with self.assertRaisesRegex(Limited,'concurrency'):
            self.ledger.reserve('premiumize',100,now=1050)
        for _ in range(3):
            self.ledger.reserve('torbox',100,now=1000)
        with self.assertRaisesRegex(Limited,'concurrency'):
            self.ledger.reserve('torbox',100,now=1000)
        status=self.ledger.status(1050)['providers']
        self.assertEqual(status['premiumize']['reserved_bytes'],110)
        self.assertEqual(status['torbox']['reserved_bytes'],330)
        self.ledger.release(token)

    def test_atomic_admission_across_threads(self):
        def attempt(_):
            try: return self.ledger.reserve('torbox',100,now=1000)
            except Limited: return None
        with ThreadPoolExecutor(max_workers=8) as pool:
            tokens=list(pool.map(attempt,range(12)))
        self.assertEqual(sum(t is not None for t in tokens),3)
        self.assertEqual(self.ledger.status(1000)['providers']['torbox']['reserved_bytes'],330)

    def test_stale_leases_expire_without_refunding_work(self):
        self.ledger.reserve('premiumize',100,now=1000)
        self.ledger.reserve('premiumize',100,now=1000+2*3600+1)
        status=self.ledger.status(9000)['providers']['premiumize']
        self.assertEqual(status['reserved_bytes'],220)
        self.assertEqual(status['active'],1)

    def test_unknown_size_is_denied_and_unknown_provider_is_strict(self):
        for size in [0,None,-1,True]:
            with self.assertRaisesRegex(Limited,'unknown_file_size'):
                self.ledger.reserve('torbox',size,now=1000)
        self.ledger.reserve('unrecognized',100,now=1000)
        self.assertEqual(self.ledger.status(1000)['providers']['premiumize']['active'],1)
        self.assertEqual(self.ledger.status(1000)['providers']['torbox']['active'],0)

    def test_probe_reservations_share_the_daily_ledger(self):
        limits={'premiumize':Policy(300*1024*1024,1,3600,60),'torbox':self.policies['torbox']}
        ledger=Ledger(self.root/'probe.sqlite',limits)
        token=ledger.reserve('premiumize',2_000_000_000,'probe',now=1000)
        ledger.release(token)
        self.assertEqual(ledger.status(1000)['providers']['premiumize']['reserved_bytes'],128*1024*1024)
        with self.assertRaisesRegex(Limited,'start_rate'):
            ledger.reserve('premiumize',2_000_000_000,'probe',now=1010)

    def test_nested_work_shares_one_reservation_and_urls_are_bound(self):
        reads=MediaReads(self.root/'media')
        reads.ledger=Ledger(self.root/'media/budget-test.sqlite',{'premiumize':Policy(1000,1,0,0),'torbox':Policy(1000,3,0,0)})
        video={'id':uuid.uuid4().hex,'url':'https://source.invalid/movie','provider':'premiumize','size':100}
        reads.register(video)
        with reads.scope(video['id'],video['url']):
            with reads.scope(video['id'],video['url']):
                self.assertEqual(reads.ledger.status()['providers']['premiumize']['active'],1)
        status=reads.ledger.status()['providers']['premiumize']
        self.assertEqual(status['reserved_bytes'],110)
        self.assertEqual(status['active'],0)
        with self.assertRaisesRegex(Limited,'unregistered_source'):
            reads.acquire(video['id'],'https://different.invalid/file')

    def test_torbox_failover_cannot_inherit_the_high_allowance(self):
        reads=MediaReads(self.root/'media')
        video={'id':'wmh-legacy-safe-id','url':'https://resolver.invalid/movie','provider':'torbox','size':100}
        reads.register(video)
        reads.check_resolved(video['id'],video['url'],'https://cdn.torbox.app/file')
        with self.assertRaisesRegex(Limited,'provider_changed'):
            reads.check_resolved(video['id'],video['url'],'https://unknown-cdn.invalid/file')
        reads.register(video)
        self.assertEqual(reads.source(video['id'],video['url'])['provider'],'premiumize')


if __name__=='__main__': unittest.main()
