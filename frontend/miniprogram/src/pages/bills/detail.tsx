import { Button, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Kv, Notice, PageHeader } from '../../runtime/ui'
import controller from './detail.controller'

export default function PageView() {
  const { data, event } = useController(controller)
  const { bill, loading, error, needsLogin, readSaving, readNotice, readError } = data
  return <View className='container'>
    <PageHeader title='账单详情' />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录后查看结算账单</Text>
      <Button type='primary' onClick={event('login', {})} loading={loading}>微信登录</Button>
    </View></View>}
    {loading && !bill && <Notice>正在读取账单…</Notice>}
    {error && <Notice tone='error' retry={event('load', {})} busy={loading}>{error}</Notice>}
    {bill && <>
      <View className='hero'><View className='hero-body'>
        <Text className='eyebrow'>账单 {bill.bill_no}</Text>
        <View className='hero-figure'>{bill.totalText}</View>
        <Text className='hero-desc'>电费 {bill.electricText} · 服务费 {bill.serviceText}</Text>
      </View></View>
      <View className='card'>
        <Kv label='来源订单'>{bill.order_no || '—'}</Kv>
        <Kv label='设备'>{bill.device_id || '—'}</Kv>
        <Kv label='出账时间'>{bill.issuedText}</Kv>
        <Kv label='已读状态'>{bill.unread ? '未读' : (bill.readText || '已读')}</Kv>
        {readSaving && <Text className='text-secondary'>正在保存已读状态…</Text>}
        {readNotice && <Text className='success'>{readNotice}</Text>}
        {readError && <Text className='error'>{readError}</Text>}
      </View>
      <Button type='primary' onClick={event('orderDetail', {})} disabled={!bill.order_no}>查看来源订单</Button>
    </>}
  </View>
}
