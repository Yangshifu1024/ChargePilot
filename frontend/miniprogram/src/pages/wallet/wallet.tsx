import { Button, Text, View } from '@tarojs/components'
import { AtIcon } from 'taro-ui'
import { useController } from '../../runtime'
import { Empty, Notice, PageHeader } from '../../runtime/ui'
import controller from './wallet.controller'

const ENTRIES = [
  { title: '余额流水', icon: 'list', action: 'goTxns' },
  { title: '钱包退款', icon: 'reload', action: 'goRefund' },
  { title: '在线卡', icon: 'credit-card', action: 'goCards' },
]

/** 钱包区分余额读取与流水读取，流水失败不撤销余额。 */
export default function PageView() {
  const { data, event } = useController(controller)
  return <View className='container'>
    <PageHeader title='我的钱包' subtitle='充值到账以支付结果为准' />
    {data.needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录后查看钱包</Text>
      <Button type='primary' onClick={event('login')}>微信登录</Button>
    </View></View>}
    {data.loading && <Notice>正在读取钱包…</Notice>}
    {data.error && <Notice tone='error' retry={event('load')}>{data.error}</Notice>}
    {data.wallet && <>
      <View className='hero'><View className='hero-body'>
        <Text className='eyebrow'>可用余额</Text>
        <View className='hero-figure'>¥{data.wallet.availableText}</View>
        <Text className='hero-desc'>冻结金额 ¥{data.wallet.frozenText}</Text>
        {data.wallet.status === 'frozen' && <Text className='banner banner-warn'>钱包已冻结，请联系客服</Text>}
        <Button type='primary' onClick={event('goRecharge')}>充值</Button>
      </View></View>
      <View className='service-grid'>{ENTRIES.map(item => <Button className='service-button' key={item.title} onClick={event(item.action)}><AtIcon value={item.icon} size={22} color='#0e6b52' /><Text>{item.title}</Text></Button>)}</View>
      <Text className='section-heading'>最近流水</Text>
      {data.txnsLoading && <Notice>正在读取流水…</Notice>}
      {data.txnsError && <Notice tone='error' retry={event('loadTxns')}>{data.txnsError}</Notice>}
      {(data.txns || []).map((item: any) => <View className='card entity' key={item.txn_no}>
        <View className='row'><Text className='entity-title'>{item.typeText}</Text><Text className={item.amount_cents > 0 ? 'success' : item.amount_cents < 0 ? 'money-out' : ''}>{item.amountText}</Text></View>
        <View className='text-secondary'>{item.timeText}</View>
      </View>)}
      {!data.txnsLoading && !data.txnsError && !data.txns?.length && <Empty icon='list'>暂无资金流水</Empty>}
      <View className='page-footnote'>充电订单退款请在订单详情查看</View>
    </>}
  </View>
}
