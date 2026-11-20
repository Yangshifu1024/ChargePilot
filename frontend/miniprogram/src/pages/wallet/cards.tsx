import { Button, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Empty, Notice, PageHeader } from '../../runtime/ui'
import controller from './cards.controller'

export default function PageView() {
  const { data, event } = useController(controller)
  const { cards, loading, error, needsLogin, busy } = data
  return <View className='container'>
    <PageHeader title='在线卡' subtitle='在线卡共用你的钱包余额' />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录查看在线卡</Text>
      <Button type='primary' onClick={event('login', {})}>登录查看在线卡</Button>
    </View></View>}
    <View className='card'><Text className='text-secondary'>首次刷卡启动，同一卡再次刷卡追加冻结套餐的时长；扣款和退款可在资金流水查看。</Text></View>
    {loading && <Notice>读取中…</Notice>}
    {error && <Notice tone='error' retry={event('load', {})}>{error}</Notice>}
    {!loading && !needsLogin && !cards?.length && <Empty icon='credit-card'>尚无绑定卡，请联系管理员核验绑定。</Empty>}
    {(cards || []).map((item: any, index: number) => <View className='pass' key={String(item?.id ?? index)}>
      <Text className='eyebrow'>在线卡</Text>
      <Text className='pass-no'>{item?.card_no}</Text>
      <Text className='text-secondary'>{item?.label}</Text>
      {item?.status === 'active' && <Button data-card={item?.card_no} data-status='lost' onClick={event('change', { card: item?.card_no, status: 'lost' })} disabled={busy}>挂失</Button>}
      <Button data-card={item?.card_no} data-status='unbound' onClick={event('change', { card: item?.card_no, status: 'unbound' })} disabled={busy}>解绑</Button>
    </View>)}
  </View>
}
